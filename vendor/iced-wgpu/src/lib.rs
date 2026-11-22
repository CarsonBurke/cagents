//! A [`wgpu`] renderer for [Iced].
//!
//! ![The native path of the Iced ecosystem](https://github.com/iced-rs/iced/blob/0525d76ff94e828b7b21634fa94a747022001c83/docs/graphs/native.png?raw=true)
//!
//! [`wgpu`] supports most modern graphics backends: Vulkan, Metal, DX11, and
//! DX12 (OpenGL and WebGL are still WIP). Additionally, it will support the
//! incoming [WebGPU API].
//!
//! Currently, `iced_wgpu` supports the following primitives:
//! - Text, which is rendered using [`glyphon`].
//! - Quads or rectangles, with rounded borders and a solid background color.
//! - Clip areas, useful to implement scrollables or hide overflowing content.
//! - Images and SVG, loaded from memory or the file system.
//! - Meshes of triangles, useful to draw geometry freely.
//!
//! [Iced]: https://github.com/iced-rs/iced
//! [`wgpu`]: https://github.com/gfx-rs/wgpu-rs
//! [WebGPU API]: https://gpuweb.github.io/gpuweb/
//! [`glyphon`]: https://github.com/grovesNL/glyphon
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/iced-rs/iced/9ab6923e943f784985e9ef9ca28b10278297225d/docs/logo.svg"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(missing_docs)]
// Keep this local patch fresh when worktrees share Cargo artifacts.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");
pub mod layer;
pub mod primitive;
pub mod settings;
pub mod window;

#[cfg(feature = "geometry")]
pub mod geometry;

mod buffer;
mod clear;
mod color;
mod engine;
mod quad;
mod text;
mod triangle;

#[cfg(any(feature = "image", feature = "svg"))]
#[path = "image/mod.rs"]
mod image;

#[cfg(not(any(feature = "image", feature = "svg")))]
#[path = "image/null.rs"]
mod image;

use buffer::Buffer;

use iced_debug as debug;
pub use iced_graphics as graphics;
pub use iced_graphics::core;
use iced_graphics::text::Raw;

pub use wgpu;

pub use engine::Engine;
pub use layer::Layer;
pub use primitive::Primitive;
pub use settings::Settings;

#[cfg(feature = "geometry")]
pub use geometry::Geometry;

use crate::core::renderer;
use crate::core::{
    Background, Color, Font, Pixels, Point, Radians, Rectangle, Size,
    Transformation, Vector, image::FilterMethod,
};
use crate::graphics::mesh;
use crate::graphics::text::{Editor, Paragraph};
use crate::graphics::{Shell, Viewport};

/// A [`wgpu`] graphics renderer for [`iced`].
///
/// [`wgpu`]: https://github.com/gfx-rs/wgpu-rs
/// [`iced`]: https://github.com/iced-rs/iced
pub struct Renderer {
    engine: Engine,

    default_font: Font,
    default_text_size: Pixels,
    layers: layer::Stack,

    quad: quad::State,
    triangle: triangle::State,
    text: text::State,
    text_viewport: text::Viewport,

    #[cfg(any(feature = "svg", feature = "image"))]
    image: image::State,

    // TODO: Centralize all the image feature handling
    #[cfg(any(feature = "svg", feature = "image"))]
    image_cache: std::cell::RefCell<image::Cache>,

    staging_belt: wgpu::util::StagingBelt,

    /// What the layers were last prepared for. Presenting them again,
    /// unchanged, for the same target prepares only what changes with time.
    prepared: Option<Prepared>,

    /// The window's last frame, when its surface can be copied to. A frame
    /// that only changes with time redraws just the regions that change and
    /// copies the rest from here.
    retained: Option<Retained>,
    clear: clear::State,
}

/// A copy of the last frame presented, drawn first and then copied to the
/// surface image.
struct Retained {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// What the frame the texture holds was drawn from, and the color it
    /// was cleared to, if it holds one.
    drawn: Option<(Prepared, Option<Color>)>,
}

impl Retained {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        size: Size<u32>,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("iced_wgpu.retained_frame"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            texture,
            view,
            drawn: None,
        }
    }

    fn size(&self) -> Size<u32> {
        Size::new(self.texture.width(), self.texture.height())
    }
}

/// What a render pass starts from.
#[derive(Debug, Clone, Copy)]
enum Load {
    /// The whole target: cleared to the color, or kept.
    Whole(Option<Color>),
    /// Only this region of the target, in physical pixels, cleared to the
    /// renderer's clear color; the rest keeps the last frame.
    Region(Rectangle),
}

/// The share of the window above which a frame redraws everything rather
/// than its damaged regions, which would then cost about as much.
const MAX_DAMAGE_SHARE: f32 = 0.5;

/// The target a layer stack was prepared for.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Prepared {
    physical_size: Size<u32>,
    scale_factor: f64,
    revision: u64,
    /// The shared text atlas's trim count once this renderer had trimmed.
    /// Another renderer's trim may let glyphs these layers draw be evicted.
    text_atlas_trims: u64,
}

impl Renderer {
    pub fn new(
        engine: Engine,
        default_font: Font,
        default_text_size: Pixels,
    ) -> Self {
        Self {
            default_font,
            default_text_size,
            layers: layer::Stack::new(),

            quad: quad::State::new(),
            triangle: triangle::State::new(
                &engine.device,
                &engine.triangle_pipeline,
            ),
            text: text::State::new(),
            text_viewport: engine.text_pipeline.create_viewport(&engine.device),

            #[cfg(any(feature = "svg", feature = "image"))]
            image: image::State::new(),

            #[cfg(any(feature = "svg", feature = "image"))]
            image_cache: std::cell::RefCell::new(engine.create_image_cache()),

            // TODO: Resize belt smartly (?)
            // It would be great if the `StagingBelt` API exposed methods
            // for introspection to detect when a resize may be worth it.
            staging_belt: wgpu::util::StagingBelt::new(
                engine.device.clone(),
                buffer::MAX_WRITE_SIZE as u64,
            ),

            prepared: None,
            retained: None,
            clear: clear::State::new(&engine.device, &engine.clear_pipeline),

            engine,
        }
    }

    /// Draws the layers to `target`. `retained` is what the frame `target`
    /// already holds was drawn from, if anything; when that is what the
    /// layers are prepared for now, only what changes with time is drawn.
    fn draw(
        &mut self,
        clear_color: Option<Color>,
        target: &wgpu::TextureView,
        viewport: &Viewport,
        retained: Option<(Prepared, Option<Color>)>,
    ) -> wgpu::CommandEncoder {
        let mut encoder = self.engine.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor {
                label: Some("iced_wgpu encoder"),
            },
        );

        // Drawings that change with time alone, such as a spinning SVG, are
        // worked out for this instant. A window may present the same layers
        // again for their next frame without drawing its widgets.
        let now = core::time::Instant::now();

        let prepared = Prepared {
            physical_size: viewport.physical_size(),
            scale_factor: viewport.scale_factor(),
            revision: self.layers.revision(),
            text_atlas_trims: self.engine.text_pipeline.trims(),
        };

        // Layers presented again for the same target keep everything they
        // uploaded last time, so only what changes with time is prepared
        // again. They are not merged again either: merging merged layers
        // can still move primitives between them. Nothing was left unused,
        // so nothing is trimmed: a trim would evict what they still draw.
        if self.prepared == Some(prepared) {
            self.prepare_animations(&mut encoder, viewport, now);
            graphics::present::lap(graphics::present::Stage::Prepare);

            // The target holds the last frame of these same layers, so only
            // what changes with time needs drawing again. Clearing a region
            // takes the clear color; without one, the frame is drawn whole.
            let damage = match clear_color {
                Some(color) if retained == Some((prepared, clear_color)) => {
                    self.damage(viewport).inspect(|_| {
                        self.clear.prepare(&self.engine.queue, color)
                    })
                }
                _ => None,
            };

            let layers = match damage {
                Some(regions) => regions
                    .into_iter()
                    .map(|region| {
                        self.render(
                            &mut encoder,
                            target,
                            Load::Region(region),
                            viewport,
                        )
                    })
                    .sum(),
                None => self.render(
                    &mut encoder,
                    target,
                    Load::Whole(clear_color),
                    viewport,
                ),
            };

            graphics::present::layers(layers);
            graphics::present::lap(graphics::present::Stage::Render);

            return encoder;
        }

        self.layers.merge();
        self.prepare(&mut encoder, viewport, now);
        graphics::present::lap(graphics::present::Stage::Prepare);
        let layers = self.render(
            &mut encoder,
            target,
            Load::Whole(clear_color),
            viewport,
        );
        graphics::present::layers(layers);
        graphics::present::lap(graphics::present::Stage::Render);

        self.quad.trim();
        self.triangle.trim();
        self.text.trim();

        // TODO: Provide window id (?)
        self.engine.trim();

        #[cfg(any(feature = "svg", feature = "image"))]
        {
            self.image.trim();
            self.image_cache.borrow_mut().trim();
        }
        graphics::present::lap(graphics::present::Stage::Trim);

        // Merging and trimming leave the revision alone; the trim count is
        // taken once this renderer's own trim has counted.
        self.prepared = Some(Prepared {
            text_atlas_trims: self.engine.text_pipeline.trims(),
            ..prepared
        });

        encoder
    }

    pub fn present(
        &mut self,
        clear_color: Option<Color>,
        _format: wgpu::TextureFormat,
        frame: &wgpu::TextureView,
        viewport: &Viewport,
    ) -> wgpu::SubmissionIndex {
        let encoder = self.draw(clear_color, frame, viewport, None);

        self.submit(encoder)
    }

    /// Presents to a surface image that can be copied to. While the layers
    /// hold something that turns, such as a spinner, the frame is drawn to
    /// a texture this renderer keeps and then copied to `frame`, so that a
    /// following frame that only turns it redraws just the regions that
    /// change. Otherwise the frame is drawn to `frame` directly.
    pub fn present_retained(
        &mut self,
        clear_color: Option<Color>,
        frame: &wgpu::Texture,
        viewport: &Viewport,
    ) -> wgpu::SubmissionIndex {
        #[cfg(any(feature = "svg", feature = "image"))]
        let animated = self
            .layers
            .iter()
            .any(|layer| layer.images.iter().any(image::is_animated));

        #[cfg(not(any(feature = "svg", feature = "image")))]
        let animated = false;

        if !animated {
            // No following frame can redraw only part of this one, so the
            // copy would be wasted, and so would the memory it is kept in.
            self.retained = None;

            let view =
                frame.create_view(&wgpu::TextureViewDescriptor::default());

            return self.present(clear_color, frame.format(), &view, viewport);
        }

        let size = viewport.physical_size();

        let retained = match self.retained.take() {
            Some(retained) if retained.size() == size => retained,
            _ => Retained::new(&self.engine.device, self.engine.format, size),
        };

        let mut encoder =
            self.draw(clear_color, &retained.view, viewport, retained.drawn);
        let drawn = self.prepared.map(|prepared| (prepared, clear_color));

        encoder.copy_texture_to_texture(
            retained.texture.as_image_copy(),
            frame.as_image_copy(),
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );

        self.retained = Some(Retained { drawn, ..retained });

        self.submit(encoder)
    }

    fn submit(
        &mut self,
        encoder: wgpu::CommandEncoder,
    ) -> wgpu::SubmissionIndex {
        self.staging_belt.finish();
        let commands = encoder.finish();
        graphics::present::lap(graphics::present::Stage::Finish);
        let submission = self.engine.queue.submit([commands]);
        self.staging_belt.recall();
        graphics::present::lap(graphics::present::Stage::Submit);
        submission
    }

    /// Renders the current surface to an offscreen buffer.
    ///
    /// Returns RGBA bytes of the texture data.
    pub fn screenshot(
        &mut self,
        viewport: &Viewport,
        background_color: Color,
    ) -> Vec<u8> {
        #[derive(Clone, Copy, Debug)]
        struct BufferDimensions {
            width: u32,
            height: u32,
            unpadded_bytes_per_row: usize,
            padded_bytes_per_row: usize,
        }

        impl BufferDimensions {
            fn new(size: Size<u32>) -> Self {
                let unpadded_bytes_per_row = size.width as usize * 4; //slice of buffer per row; always RGBA
                let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize; //256
                let padded_bytes_per_row_padding = (alignment
                    - unpadded_bytes_per_row % alignment)
                    % alignment;
                let padded_bytes_per_row =
                    unpadded_bytes_per_row + padded_bytes_per_row_padding;

                Self {
                    width: size.width,
                    height: size.height,
                    unpadded_bytes_per_row,
                    padded_bytes_per_row,
                }
            }
        }

        let dimensions = BufferDimensions::new(viewport.physical_size());

        let texture_extent = wgpu::Extent3d {
            width: dimensions.width,
            height: dimensions.height,
            depth_or_array_layers: 1,
        };

        let texture =
            self.engine.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("iced_wgpu.offscreen.source_texture"),
                size: texture_extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.engine.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder =
            self.draw(Some(background_color), &view, viewport, None);

        let texture = crate::color::convert(
            &self.engine.device,
            &mut encoder,
            texture,
            if graphics::color::GAMMA_CORRECTION {
                wgpu::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            },
        );

        let output_buffer =
            self.engine.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("iced_wgpu.offscreen.output_texture_buffer"),
                size: (dimensions.padded_bytes_per_row
                    * dimensions.height as usize) as u64,
                usage: wgpu::BufferUsages::MAP_READ
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(dimensions.padded_bytes_per_row as u32),
                    rows_per_image: None,
                },
            },
            texture_extent,
        );

        self.staging_belt.finish();
        let index = self.engine.queue.submit([encoder.finish()]);
        self.staging_belt.recall();

        let slice = output_buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});

        let _ = self.engine.device.poll(wgpu::PollType::Wait {
            submission_index: Some(index),
            timeout: None,
        });

        let mapped_buffer = slice.get_mapped_range();

        mapped_buffer.chunks(dimensions.padded_bytes_per_row).fold(
            vec![],
            |mut acc, row| {
                acc.extend(&row[..dimensions.unpadded_bytes_per_row]);
                acc
            },
        )
    }

    fn prepare(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        viewport: &Viewport,
        now: core::time::Instant,
    ) {
        let scale_factor = viewport.scale_factor();

        self.text_viewport
            .update(&self.engine.queue, viewport.physical_size());

        let physical_bounds = Rectangle::<f32>::from(Rectangle::with_size(
            viewport.physical_size(),
        ));

        for layer in self.layers.iter() {
            let clip_bounds = layer.bounds * scale_factor as f32;

            if physical_bounds
                .intersection(&clip_bounds)
                .and_then(Rectangle::snap)
                .is_none()
            {
                continue;
            }

            if !layer.quads.is_empty() {
                let prepare_span = debug::prepare(debug::Primitive::Quad);

                self.quad.prepare(
                    &self.engine.quad_pipeline,
                    &self.engine.device,
                    &mut self.staging_belt,
                    encoder,
                    &layer.quads,
                    viewport.projection(),
                    scale_factor as f32,
                );

                prepare_span.finish();
            }

            if !layer.triangles.is_empty() {
                let prepare_span = debug::prepare(debug::Primitive::Triangle);

                self.triangle.prepare(
                    &self.engine.triangle_pipeline,
                    &self.engine.device,
                    &mut self.staging_belt,
                    encoder,
                    &layer.triangles,
                    Transformation::scale(scale_factor as f32),
                    viewport.physical_size(),
                );

                prepare_span.finish();
            }

            if !layer.primitives.is_empty() {
                let prepare_span = debug::prepare(debug::Primitive::Shader);

                let mut primitive_storage = self
                    .engine
                    .primitive_storage
                    .write()
                    .expect("Write primitive storage");

                for instance in &layer.primitives {
                    instance.primitive.prepare(
                        &mut primitive_storage,
                        &self.engine.device,
                        &self.engine.queue,
                        self.engine.format,
                        &instance.bounds,
                        viewport,
                    );
                }

                prepare_span.finish();
            }

            #[cfg(any(feature = "svg", feature = "image"))]
            if !layer.images.is_empty() {
                let prepare_span = debug::prepare(debug::Primitive::Image);

                self.image.prepare(
                    &self.engine.image_pipeline,
                    &self.engine.device,
                    &mut self.staging_belt,
                    encoder,
                    &mut self.image_cache.borrow_mut(),
                    &layer.images,
                    viewport.projection(),
                    scale_factor as f32,
                    now,
                );

                prepare_span.finish();
            }

            if !layer.text.is_empty() {
                let prepare_span = debug::prepare(debug::Primitive::Text);

                self.text.prepare(
                    &self.engine.text_pipeline,
                    &self.engine.device,
                    &self.engine.queue,
                    &self.text_viewport,
                    encoder,
                    &layer.text,
                    layer.bounds,
                    Transformation::scale(scale_factor as f32),
                );

                prepare_span.finish();
            }
        }
    }

    /// Prepares layers that were prepared for this viewport before, and not
    /// reset since, for another frame: only their images that change with
    /// time, and their custom primitives, which may expect a `prepare` per
    /// frame. Everything else keeps what it uploaded last time.
    ///
    /// Images are prepared per layer in the order [`Self::prepare`] visits
    /// them, so a layer's index here matches the one it had there and the
    /// one [`Self::render`] draws.
    fn prepare_animations(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        viewport: &Viewport,
        now: core::time::Instant,
    ) {
        let scale_factor = viewport.scale_factor();

        let physical_bounds = Rectangle::<f32>::from(Rectangle::with_size(
            viewport.physical_size(),
        ));

        #[cfg(any(feature = "svg", feature = "image"))]
        let mut image_layer = 0;

        for layer in self.layers.iter() {
            let clip_bounds = layer.bounds * scale_factor as f32;

            if physical_bounds
                .intersection(&clip_bounds)
                .and_then(Rectangle::snap)
                .is_none()
            {
                continue;
            }

            if !layer.primitives.is_empty() {
                let mut primitive_storage = self
                    .engine
                    .primitive_storage
                    .write()
                    .expect("Write primitive storage");

                for instance in &layer.primitives {
                    instance.primitive.prepare(
                        &mut primitive_storage,
                        &self.engine.device,
                        &self.engine.queue,
                        self.engine.format,
                        &instance.bounds,
                        viewport,
                    );
                }
            }

            #[cfg(any(feature = "svg", feature = "image"))]
            if !layer.images.is_empty() {
                if layer.images.iter().any(image::is_animated) {
                    let prepare_span = debug::prepare(debug::Primitive::Image);

                    self.image.prepare_at(
                        image_layer,
                        &self.engine.image_pipeline,
                        &self.engine.device,
                        &mut self.staging_belt,
                        encoder,
                        &mut self.image_cache.borrow_mut(),
                        &layer.images,
                        viewport.projection(),
                        scale_factor as f32,
                        now,
                    );

                    prepare_span.finish();
                }

                image_layer += 1;
            }
        }
    }

    /// The regions, in physical pixels, that change with time when these
    /// layers are presented again: turning drawings, and custom primitives,
    /// which are prepared again for every frame. `None` when they cover so
    /// much of the window, or reach into so many layers, that drawing it
    /// whole costs about the same.
    fn damage(&self, viewport: &Viewport) -> Option<Vec<Rectangle>> {
        let scale_factor = viewport.scale_factor() as f32;
        let window = Rectangle::<f32>::from(Rectangle::with_size(
            viewport.physical_size(),
        ));

        let mut damage = Vec::new();

        for layer in self.layers.iter() {
            let Some(clip) =
                window.intersection(&(layer.bounds * scale_factor))
            else {
                continue;
            };

            // A layer with a turning drawing has all its images prepared
            // again (`prepare_animations`), so a raster image that finished
            // loading since the last frame appears in it now.
            #[cfg(any(feature = "svg", feature = "image"))]
            if layer.images.iter().any(image::is_animated) {
                // A pixel more for the edges' antialiasing.
                damage.extend(layer.images.iter().filter_map(|drawing| {
                    image::damage(drawing)
                        .map(|region| (region * scale_factor).expand(1.0))
                        .and_then(|region| region.intersection(&clip))
                }));
            }

            damage.extend(layer.primitives.iter().filter_map(|instance| {
                (instance.bounds * scale_factor).intersection(&clip)
            }));
        }

        let damage = graphics::damage::group(damage, window);
        let area: f32 = damage.iter().map(Rectangle::area).sum();

        if area > window.area() * MAX_DAMAGE_SHARE {
            return None;
        }

        // Each region records every layer that reaches into it, so regions
        // that each cross a large layer, such as a transcript with spinners
        // far apart, can record more than the whole frame would.
        let visible = || {
            self.layers.iter().filter_map(|layer| {
                window.intersection(&(layer.bounds * scale_factor))
            })
        };
        let whole = visible().count();
        let recorded: usize = damage
            .iter()
            .map(|region| {
                visible().filter(|bounds| bounds.intersects(region)).count()
            })
            .sum();

        (recorded < whole).then_some(damage)
    }

    /// Records the layers into render passes on `frame`, and returns how
    /// many layers it drew. With [`Load::Region`], it clears that region and
    /// draws only the layers that reach into it, clipped to it.
    fn render(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        frame: &wgpu::TextureView,
        load: Load,
        viewport: &Viewport,
    ) -> usize {
        use std::mem::ManuallyDrop;

        let (clear_color, region) = match load {
            Load::Whole(clear_color) => (clear_color, None),
            Load::Region(region) => (None, Some(region)),
        };

        let mut drawn = 0;

        let mut render_pass = ManuallyDrop::new(encoder.begin_render_pass(
            &wgpu::RenderPassDescriptor {
                label: Some("iced_wgpu render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: match clear_color {
                            Some(background_color) => wgpu::LoadOp::Clear({
                                let [r, g, b, a] =
                                    graphics::color::pack(background_color)
                                        .components();

                                wgpu::Color {
                                    r: f64::from(r),
                                    g: f64::from(g),
                                    b: f64::from(b),
                                    a: f64::from(a),
                                }
                            }),
                            None => wgpu::LoadOp::Load,
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            },
        ));

        if let Some(scissor) = region.and_then(Rectangle::snap) {
            render_pass.set_scissor_rect(
                scissor.x,
                scissor.y,
                scissor.width,
                scissor.height,
            );
            self.clear
                .render(&self.engine.clear_pipeline, &mut render_pass);
        }

        let mut quad_layer = 0;
        let mut mesh_layer = 0;
        let mut text_layer = 0;

        #[cfg(any(feature = "svg", feature = "image"))]
        let mut image_layer = 0;

        let scale_factor = viewport.scale_factor();
        let physical_bounds = Rectangle::<f32>::from(Rectangle::with_size(
            viewport.physical_size(),
        ));

        let scale = Transformation::scale(scale_factor as f32);

        for layer in self.layers.iter() {
            let Some(physical_bounds) = physical_bounds
                .intersection(&(layer.bounds * scale_factor as f32))
            else {
                continue;
            };

            // Skipped here, as when it was prepared.
            let Some(scissor_rect) = physical_bounds.snap() else {
                continue;
            };

            // A layer outside the region keeps what the last frame drew, but
            // its prepared data still takes its place in each pipeline.
            let Some((physical_bounds, scissor_rect)) = (match region {
                Some(region) => physical_bounds
                    .intersection(&region)
                    .and_then(|bounds| Some((bounds, bounds.snap()?))),
                None => Some((physical_bounds, scissor_rect)),
            }) else {
                if !layer.quads.is_empty() {
                    quad_layer += 1;
                }

                mesh_layer += layer
                    .triangles
                    .iter()
                    .filter(|item| matches!(item, triangle::Item::Group { .. }))
                    .count();

                #[cfg(any(feature = "svg", feature = "image"))]
                if !layer.images.is_empty() {
                    image_layer += 1;
                }

                text_layer += layer
                    .text
                    .iter()
                    .filter(|item| matches!(item, text::Item::Group { .. }))
                    .count();

                continue;
            };

            drawn += 1;

            if !layer.quads.is_empty() {
                let render_span = debug::render(debug::Primitive::Quad);
                self.quad.render(
                    &self.engine.quad_pipeline,
                    quad_layer,
                    scissor_rect,
                    &layer.quads,
                    &mut render_pass,
                );
                render_span.finish();

                quad_layer += 1;
            }

            if !layer.triangles.is_empty() {
                let _ = ManuallyDrop::into_inner(render_pass);

                let render_span = debug::render(debug::Primitive::Triangle);
                mesh_layer += self.triangle.render(
                    &self.engine.triangle_pipeline,
                    encoder,
                    frame,
                    mesh_layer,
                    &layer.triangles,
                    physical_bounds,
                    scale,
                );
                render_span.finish();

                render_pass = ManuallyDrop::new(encoder.begin_render_pass(
                    &wgpu::RenderPassDescriptor {
                        label: Some("iced_wgpu render pass"),
                        color_attachments: &[Some(
                            wgpu::RenderPassColorAttachment {
                                view: frame,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Load,
                                    store: wgpu::StoreOp::Store,
                                },
                            },
                        )],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    },
                ));
            }

            if !layer.primitives.is_empty() {
                let render_span = debug::render(debug::Primitive::Shader);

                let primitive_storage = self
                    .engine
                    .primitive_storage
                    .read()
                    .expect("Read primitive storage");

                let mut need_render = Vec::new();

                for instance in &layer.primitives {
                    let bounds = instance.bounds * scale;

                    if let Some(clip_bounds) = (instance.bounds * scale)
                        .intersection(&physical_bounds)
                        .and_then(Rectangle::snap)
                    {
                        render_pass.set_viewport(
                            bounds.x,
                            bounds.y,
                            bounds.width,
                            bounds.height,
                            0.0,
                            1.0,
                        );

                        render_pass.set_scissor_rect(
                            clip_bounds.x,
                            clip_bounds.y,
                            clip_bounds.width,
                            clip_bounds.height,
                        );

                        let drawn = instance
                            .primitive
                            .draw(&primitive_storage, &mut render_pass);

                        if !drawn {
                            need_render.push((instance, clip_bounds));
                        }
                    }
                }

                render_pass.set_viewport(
                    0.0,
                    0.0,
                    viewport.physical_width() as f32,
                    viewport.physical_height() as f32,
                    0.0,
                    1.0,
                );

                render_pass.set_scissor_rect(
                    0,
                    0,
                    viewport.physical_width(),
                    viewport.physical_height(),
                );

                if !need_render.is_empty() {
                    let _ = ManuallyDrop::into_inner(render_pass);

                    for (instance, clip_bounds) in need_render {
                        instance.primitive.render(
                            &primitive_storage,
                            encoder,
                            frame,
                            &clip_bounds,
                        );
                    }

                    render_pass = ManuallyDrop::new(encoder.begin_render_pass(
                        &wgpu::RenderPassDescriptor {
                            label: Some("iced_wgpu render pass"),
                            color_attachments: &[Some(
                                wgpu::RenderPassColorAttachment {
                                    view: frame,
                                    depth_slice: None,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Load,
                                        store: wgpu::StoreOp::Store,
                                    },
                                },
                            )],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        },
                    ));
                }

                render_span.finish();
            }

            #[cfg(any(feature = "svg", feature = "image"))]
            if !layer.images.is_empty() {
                let render_span = debug::render(debug::Primitive::Image);
                self.image.render(
                    &self.engine.image_pipeline,
                    image_layer,
                    scissor_rect,
                    &mut render_pass,
                );
                render_span.finish();

                image_layer += 1;
            }

            if !layer.text.is_empty() {
                let render_span = debug::render(debug::Primitive::Text);
                text_layer += self.text.render(
                    &self.engine.text_pipeline,
                    &self.text_viewport,
                    text_layer,
                    &layer.text,
                    scissor_rect,
                    &mut render_pass,
                );
                render_span.finish();
            }
        }

        let _ = ManuallyDrop::into_inner(render_pass);

        debug::layers_rendered(|| {
            self.layers
                .iter()
                .filter(|layer| {
                    !layer.is_empty()
                        && physical_bounds
                            .intersection(&(layer.bounds * scale_factor as f32))
                            .is_some_and(|viewport| viewport.snap().is_some())
                })
                .count()
        });

        drawn
    }

    fn draw_overlay(
        &mut self,
        overlay: &[impl AsRef<str>],
        viewport: &Viewport,
    ) {
        use crate::core::Renderer as _;
        use crate::core::alignment;
        use crate::core::text::Renderer as _;

        self.with_layer(
            Rectangle::with_size(viewport.logical_size()),
            |renderer| {
                for (i, line) in overlay.iter().enumerate() {
                    let text = crate::core::Text {
                        content: line.as_ref().to_owned(),
                        bounds: viewport.logical_size(),
                        size: Pixels(20.0),
                        line_height: core::text::LineHeight::default(),
                        font: Font::MONOSPACE,
                        align_x: core::text::Alignment::Left,
                        align_y: alignment::Vertical::Top,
                        shaping: core::text::Shaping::Advanced,
                        wrapping: core::text::Wrapping::Word,
                        ellipsize: core::text::Ellipsize::None,
                    };

                    renderer.fill_text(
                        text.clone(),
                        Point::new(11.0, 11.0 + 25.0 * i as f32),
                        Color::from_rgba(0.9, 0.9, 0.9, 1.0),
                        Rectangle::with_size(Size::INFINITE),
                    );

                    renderer.fill_text(
                        text,
                        Point::new(11.0, 11.0 + 25.0 * i as f32)
                            + Vector::new(-1.0, -1.0),
                        Color::BLACK,
                        Rectangle::with_size(Size::INFINITE),
                    );
                }
            },
        );
    }
}

impl core::Renderer for Renderer {
    fn start_layer(&mut self, bounds: Rectangle) {
        self.layers.push_clip(bounds);
    }

    fn end_layer(&mut self) {
        self.layers.pop_clip();
    }

    fn start_transformation(&mut self, transformation: Transformation) {
        self.layers.push_transformation(transformation);
    }

    fn end_transformation(&mut self) {
        self.layers.pop_transformation();
    }

    fn fill_quad(
        &mut self,
        quad: core::renderer::Quad,
        background: impl Into<Background>,
    ) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_quad(quad, background.into(), transformation);
    }

    fn reset(&mut self, new_bounds: Rectangle) {
        self.layers.reset(new_bounds);
    }

    fn allocate_image(
        &mut self,
        _handle: &core::image::Handle,
        _callback: impl FnOnce(Result<core::image::Allocation, core::image::Error>)
        + Send
        + 'static,
    ) {
        #[cfg(feature = "image")]
        self.image_cache
            .get_mut()
            .allocate_image(_handle, _callback);
    }
}

impl core::text::Renderer for Renderer {
    type Font = Font;
    type Paragraph = Paragraph;
    type Editor = Editor;
    type Raw = Raw;

    const ICON_FONT: Font = Font::with_name("Iced-Icons");
    const CHECKMARK_ICON: char = '\u{f00c}';
    const ARROW_DOWN_ICON: char = '\u{e800}';
    const ICED_LOGO: char = '\u{e801}';
    const SCROLL_UP_ICON: char = '\u{e802}';
    const SCROLL_DOWN_ICON: char = '\u{e803}';
    const SCROLL_LEFT_ICON: char = '\u{e804}';
    const SCROLL_RIGHT_ICON: char = '\u{e805}';

    fn default_font(&self) -> Self::Font {
        self.default_font
    }

    fn default_size(&self) -> Pixels {
        self.default_text_size
    }

    fn fill_paragraph(
        &mut self,
        text: &Self::Paragraph,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    ) {
        let (layer, transformation) = self.layers.current_mut();

        layer.draw_paragraph(
            text,
            position,
            color,
            clip_bounds,
            transformation,
        );
    }

    fn fill_editor(
        &mut self,
        editor: &Self::Editor,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    ) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_editor(editor, position, color, clip_bounds, transformation);
    }

    fn fill_text(
        &mut self,
        text: core::Text,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    ) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_text(text, position, color, clip_bounds, transformation);
    }

    fn fill_raw(&mut self, raw: Self::Raw) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_text_raw(raw, transformation);
    }
}

impl graphics::text::Renderer for Renderer {
    fn fill_raw(&mut self, raw: graphics::text::Raw) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_text_raw(raw, transformation);
    }
}

#[cfg(feature = "image")]
impl core::image::Renderer for Renderer {
    type Handle = core::image::Handle;

    fn load_image(
        &self,
        handle: &Self::Handle,
    ) -> Result<core::image::Allocation, core::image::Error> {
        self.image_cache.borrow_mut().load_image(
            &self.engine.device,
            &self.engine.queue,
            handle,
        )
    }

    fn measure_image(&self, handle: &Self::Handle) -> Option<core::Size<u32>> {
        self.image_cache.borrow_mut().measure_image(handle)
    }

    fn draw_image(
        &mut self,
        image: core::Image,
        bounds: Rectangle,
        clip_bounds: Rectangle,
    ) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_raster(image, bounds, clip_bounds, transformation);
    }
}

#[cfg(feature = "svg")]
impl core::svg::Renderer for Renderer {
    fn measure_svg(&self, handle: &core::svg::Handle) -> core::Size<u32> {
        self.image_cache.borrow_mut().measure_svg(handle)
    }

    fn draw_svg(
        &mut self,
        svg: core::Svg,
        bounds: Rectangle,
        clip_bounds: Rectangle,
    ) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_svg(svg, bounds, clip_bounds, transformation);
    }
}

impl graphics::mesh::Renderer for Renderer {
    fn draw_mesh(&mut self, mesh: graphics::Mesh) {
        debug_assert!(
            !mesh.indices().is_empty(),
            "Mesh must not have empty indices"
        );

        debug_assert!(
            mesh.indices().len().is_multiple_of(3),
            "Mesh indices length must be a multiple of 3"
        );

        let (layer, transformation) = self.layers.current_mut();
        layer.draw_mesh(mesh, transformation);
    }

    fn draw_mesh_cache(&mut self, cache: mesh::Cache) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_mesh_cache(cache, transformation);
    }
}

#[cfg(feature = "geometry")]
impl graphics::geometry::Renderer for Renderer {
    type Geometry = Geometry;
    type Frame = geometry::Frame;

    fn new_frame(&self, bounds: Rectangle) -> Self::Frame {
        geometry::Frame::new(bounds)
    }

    fn draw_geometry(&mut self, geometry: Self::Geometry) {
        let (layer, transformation) = self.layers.current_mut();

        match geometry {
            Geometry::Live {
                meshes,
                images,
                text,
            } => {
                layer.draw_mesh_group(meshes, transformation);

                for image in images {
                    layer.draw_image(image, transformation);
                }

                layer.draw_text_group(text, transformation);
            }
            Geometry::Cached(cache) => {
                if let Some(meshes) = cache.meshes {
                    layer.draw_mesh_cache(meshes, transformation);
                }

                if let Some(images) = cache.images {
                    for image in images.iter().cloned() {
                        layer.draw_image(image, transformation);
                    }
                }

                if let Some(text) = cache.text {
                    layer.draw_text_cache(text, transformation);
                }
            }
        }
    }
}

impl primitive::Renderer for Renderer {
    fn draw_primitive(&mut self, bounds: Rectangle, primitive: impl Primitive) {
        let (layer, transformation) = self.layers.current_mut();
        layer.draw_primitive(bounds, primitive, transformation);
    }
}

impl graphics::compositor::Default for crate::Renderer {
    type Compositor = window::Compositor;
}

impl renderer::Headless for Renderer {
    async fn new(
        default_font: Font,
        default_text_size: Pixels,
        backend: Option<&str>,
    ) -> Option<Self> {
        if backend.is_some_and(|backend| backend != "wgpu") {
            return None;
        }

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env()
                .unwrap_or(wgpu::Backends::PRIMARY),
            flags: wgpu::InstanceFlags::empty(),
            ..wgpu::InstanceDescriptor::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .ok()?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("iced_wgpu [headless]"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits {
                    max_bind_groups: 2,
                    ..wgpu::Limits::default()
                },
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .ok()?;

        let engine = Engine::new(
            &adapter,
            device,
            queue,
            if graphics::color::GAMMA_CORRECTION {
                wgpu::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            },
            Some(graphics::Antialiasing::MSAAx4),
            Shell::headless(),
        );

        Some(Self::new(engine, default_font, default_text_size))
    }

    fn name(&self) -> String {
        "wgpu".to_owned()
    }

    fn screenshot(
        &mut self,
        size: Size<u32>,
        scale_factor: f32,
        background_color: Color,
    ) -> Vec<u8> {
        self.screenshot(
            &Viewport::with_physical_size(size, scale_factor as f64),
            background_color,
        )
    }
}
