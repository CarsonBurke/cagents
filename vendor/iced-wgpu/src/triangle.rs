//! Draw meshes of triangles.
mod msaa;

use crate::Buffer;
use crate::core::{Point, Rectangle, Size, Transformation, Vector};
use crate::graphics::Antialiasing;
use crate::graphics::mesh::{self, Mesh};

use rustc_hash::FxHashMap;
use std::collections::hash_map;
use std::sync::Weak;

const INITIAL_INDEX_COUNT: usize = 1_000;
const INITIAL_VERTEX_COUNT: usize = 1_000;

pub type Batch = Vec<Item>;

#[derive(Debug)]
pub enum Item {
    Group {
        transformation: Transformation,
        meshes: Vec<Mesh>,
    },
    Cached {
        transformation: Transformation,
        cache: mesh::Cache,
    },
}

#[derive(Debug)]
struct Upload {
    layer: Layer,
    transformation: Transformation,
    version: usize,
    batch: Weak<[Mesh]>,
}

#[derive(Debug, Default)]
pub struct Storage {
    uploads: FxHashMap<mesh::Id, Upload>,
}

impl Storage {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, cache: &mesh::Cache) -> Option<&Upload> {
        if cache.is_empty() {
            return None;
        }

        self.uploads.get(&cache.id())
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        solid: &solid::Pipeline,
        gradient: &gradient::Pipeline,
        cache: &mesh::Cache,
        new_transformation: Transformation,
    ) {
        match self.uploads.entry(cache.id()) {
            hash_map::Entry::Occupied(entry) => {
                let upload = entry.into_mut();

                if !cache.is_empty()
                    && (upload.version != cache.version()
                        || upload.transformation != new_transformation)
                {
                    upload.layer.prepare(
                        device,
                        encoder,
                        belt,
                        solid,
                        gradient,
                        cache.batch(),
                        new_transformation,
                    );

                    upload.batch = cache.downgrade();
                    upload.version = cache.version();
                    upload.transformation = new_transformation;
                }
            }
            hash_map::Entry::Vacant(entry) => {
                let mut layer = Layer::new(device, solid, gradient);

                layer.prepare(
                    device,
                    encoder,
                    belt,
                    solid,
                    gradient,
                    cache.batch(),
                    new_transformation,
                );

                let _ = entry.insert(Upload {
                    layer,
                    transformation: new_transformation,
                    version: 0,
                    batch: cache.downgrade(),
                });

                log::debug!(
                    "New mesh upload: {:?} (total: {})",
                    cache.id(),
                    self.uploads.len()
                );
            }
        }
    }

    pub fn trim(&mut self) {
        self.uploads
            .retain(|_id, upload| upload.batch.strong_count() > 0);
    }
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    msaa: Option<msaa::Pipeline>,
    solid: solid::Pipeline,
    gradient: gradient::Pipeline,
}

pub struct State {
    msaa: Option<msaa::State>,
    layers: Vec<Layer>,
    prepare_layer: usize,
    storage: Storage,
}

impl State {
    pub fn new(device: &wgpu::Device, pipeline: &Pipeline) -> Self {
        Self {
            msaa: pipeline
                .msaa
                .as_ref()
                .map(|pipeline| msaa::State::new(device, pipeline)),
            layers: Vec::new(),
            prepare_layer: 0,
            storage: Storage::new(),
        }
    }

    pub fn prepare(
        &mut self,
        pipeline: &Pipeline,
        device: &wgpu::Device,
        belt: &mut wgpu::util::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        items: &[Item],
        scale: Transformation,
        target_size: Size<u32>,
    ) {
        let projection = if let Some((state, pipeline)) =
            self.msaa.as_mut().zip(pipeline.msaa.as_ref())
        {
            state.prepare(device, encoder, belt, pipeline, target_size) * scale
        } else {
            Transformation::orthographic(target_size.width, target_size.height)
                * scale
        };

        for item in items {
            match item {
                Item::Group {
                    transformation,
                    meshes,
                } => {
                    if self.layers.len() <= self.prepare_layer {
                        self.layers.push(Layer::new(
                            device,
                            &pipeline.solid,
                            &pipeline.gradient,
                        ));
                    }

                    let layer = &mut self.layers[self.prepare_layer];
                    layer.prepare(
                        device,
                        encoder,
                        belt,
                        &pipeline.solid,
                        &pipeline.gradient,
                        meshes,
                        projection * *transformation,
                    );

                    self.prepare_layer += 1;
                }
                Item::Cached {
                    transformation,
                    cache,
                } => {
                    self.storage.prepare(
                        device,
                        encoder,
                        belt,
                        &pipeline.solid,
                        &pipeline.gradient,
                        cache,
                        projection * *transformation,
                    );
                }
            }
        }
    }

    pub fn render(
        &mut self,
        pipeline: &Pipeline,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        start: usize,
        batch: &Batch,
        bounds: Rectangle,
        screen_transformation: Transformation,
    ) -> usize {
        let mut layer_count = 0;

        let items = batch.iter().filter_map(|item| match item {
            Item::Group {
                transformation,
                meshes,
            } => {
                let layer = &self.layers[start + layer_count];
                layer_count += 1;

                Some((
                    layer,
                    meshes.as_slice(),
                    screen_transformation * *transformation,
                ))
            }
            Item::Cached {
                transformation,
                cache,
            } => {
                let upload = self.storage.get(cache)?;

                Some((
                    &upload.layer,
                    cache.batch(),
                    screen_transformation * *transformation,
                ))
            }
        });

        render(
            encoder,
            target,
            self.msaa.as_ref().zip(pipeline.msaa.as_ref()),
            &pipeline.solid,
            &pipeline.gradient,
            bounds,
            items,
        );

        layer_count
    }

    pub fn trim(&mut self) {
        self.storage.trim();

        self.prepare_layer = 0;
    }
}

impl Pipeline {
    /// The size of the multisampled targets every window shares, when there
    /// are any; see [`msaa::Pipeline::size`].
    pub fn msaa_size(&self) -> Option<Size<u32>> {
        self.msaa.as_ref().and_then(msaa::Pipeline::size)
    }

    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        antialiasing: Option<Antialiasing>,
    ) -> Pipeline {
        Pipeline {
            msaa: antialiasing.map(|a| msaa::Pipeline::new(device, format, a)),
            solid: solid::Pipeline::new(device, format, antialiasing),
            gradient: gradient::Pipeline::new(device, format, antialiasing),
        }
    }
}

fn render<'a>(
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    mut msaa: Option<(&msaa::State, &msaa::Pipeline)>,
    solid: &solid::Pipeline,
    gradient: &gradient::Pipeline,
    bounds: Rectangle,
    group: impl Iterator<Item = (&'a Layer, &'a [Mesh], Transformation)>,
) {
    // Every mesh drawn here is clipped to the layer, so the layer's pixels
    // are all this pass changes.
    let Some(region) = bounds.snap() else {
        return;
    };

    {
        let (mut render_pass, attachment) = if let Some((_state, pipeline)) =
            &mut msaa
        {
            let (pass, size) = pipeline.render_pass(encoder);

            (pass, Some(size))
        } else {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("iced_wgpu.triangle.render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            (pass, None)
        };

        for (layer, meshes, transformation) in group {
            layer.render(
                solid,
                gradient,
                meshes,
                bounds,
                transformation,
                &mut render_pass,
            );
        }

        // The OpenGL backend resolves the multisampled attachment with a
        // framebuffer blit when the pass ends, and a blit only writes inside
        // the scissor still set: that of the last mesh drawn. The rest of the
        // resolve texture, shared by every window, would keep what earlier
        // passes resolved there, and the blend below would draw it again, so
        // rings and arcs from other layers, frames and windows piled up
        // wherever no later mesh covered them. Resolving the whole layer
        // writes every pixel the blend reads.
        if let Some(attachment) = attachment {
            let scissor = clamp(region, attachment);

            render_pass.set_scissor_rect(
                scissor.x,
                scissor.y,
                scissor.width,
                scissor.height,
            );
        }
    }

    if let Some((state, pipeline)) = msaa {
        state.render(pipeline, encoder, target, region);
    }
}

/// `region` without what lies past `size`.
fn clamp(region: Rectangle<u32>, size: Size<u32>) -> Rectangle<u32> {
    let x = region.x.min(size.width);
    let y = region.y.min(size.height);

    Rectangle {
        x,
        y,
        width: region.width.min(size.width - x),
        height: region.height.min(size.height - y),
    }
}

#[derive(Debug)]
pub struct Layer {
    index_buffer: Buffer<u32>,
    solid: solid::Layer,
    gradient: gradient::Layer,
}

impl Layer {
    fn new(
        device: &wgpu::Device,
        solid: &solid::Pipeline,
        gradient: &gradient::Pipeline,
    ) -> Self {
        Self {
            index_buffer: Buffer::new(
                device,
                "iced_wgpu.triangle.index_buffer",
                INITIAL_INDEX_COUNT,
                wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            ),
            solid: solid::Layer::new(device, &solid.constants_layout),
            gradient: gradient::Layer::new(device, &gradient.constants_layout),
        }
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        solid: &solid::Pipeline,
        gradient: &gradient::Pipeline,
        meshes: &[Mesh],
        transformation: Transformation,
    ) {
        // Count the total amount of vertices & indices we need to handle
        let count = mesh::attribute_count_of(meshes);

        // Then we ensure the current attribute buffers are big enough, resizing if necessary.
        // We are not currently using the return value of these functions as we have no system in
        // place to calculate mesh diff, or to know whether or not that would be more performant for
        // the majority of use cases. Therefore we will write GPU data every frame (for now).
        let _ = self.index_buffer.resize(device, count.indices);
        let _ = self.solid.vertices.resize(device, count.solid_vertices);
        let _ = self
            .gradient
            .vertices
            .resize(device, count.gradient_vertices);

        if self.solid.uniforms.resize(device, count.solids) {
            self.solid.constants = solid::Layer::bind_group(
                device,
                &self.solid.uniforms.raw,
                &solid.constants_layout,
            );
        }

        if self.gradient.uniforms.resize(device, count.gradients) {
            self.gradient.constants = gradient::Layer::bind_group(
                device,
                &self.gradient.uniforms.raw,
                &gradient.constants_layout,
            );
        }

        let mut solid_vertex_offset = 0;
        let mut solid_uniform_offset = 0;
        let mut gradient_vertex_offset = 0;
        let mut gradient_uniform_offset = 0;
        let mut index_offset = 0;

        for mesh in meshes {
            let clip_bounds = mesh.clip_bounds() * transformation;
            let snap_distance = clip_bounds
                .snap()
                .map(|snapped_bounds| {
                    Point::new(snapped_bounds.x as f32, snapped_bounds.y as f32)
                        - clip_bounds.position()
                })
                .unwrap_or(Vector::ZERO);

            let uniforms = Uniforms::new(
                transformation
                    * mesh.transformation()
                    * Transformation::translate(
                        snap_distance.x,
                        snap_distance.y,
                    ),
            );

            let indices = mesh.indices();

            index_offset +=
                self.index_buffer
                    .write(encoder, belt, index_offset, indices);

            match mesh {
                Mesh::Solid { buffers, .. } => {
                    solid_vertex_offset += self.solid.vertices.write(
                        encoder,
                        belt,
                        solid_vertex_offset,
                        &buffers.vertices,
                    );

                    solid_uniform_offset += self.solid.uniforms.write(
                        encoder,
                        belt,
                        solid_uniform_offset,
                        &[uniforms],
                    )
                }
                Mesh::Gradient { buffers, .. } => {
                    gradient_vertex_offset += self.gradient.vertices.write(
                        encoder,
                        belt,
                        gradient_vertex_offset,
                        &buffers.vertices,
                    );

                    gradient_uniform_offset += self.gradient.uniforms.write(
                        encoder,
                        belt,
                        gradient_uniform_offset,
                        &[uniforms],
                    );
                }
            }
        }
    }

    fn render<'a>(
        &'a self,
        solid: &'a solid::Pipeline,
        gradient: &'a gradient::Pipeline,
        meshes: &[Mesh],
        bounds: Rectangle,
        transformation: Transformation,
        render_pass: &mut wgpu::RenderPass<'a>,
    ) {
        let mut num_solids = 0;
        let mut num_gradients = 0;
        let mut solid_offset = 0;
        let mut gradient_offset = 0;
        let mut index_offset = 0;
        let mut last_is_solid = None;

        for mesh in meshes {
            let Some(clip_bounds) = bounds
                .intersection(&(mesh.clip_bounds() * transformation))
                .and_then(Rectangle::snap)
            else {
                match mesh {
                    Mesh::Solid { buffers, .. } => {
                        solid_offset += buffers.vertices.len();
                        num_solids += 1;
                    }
                    Mesh::Gradient { buffers, .. } => {
                        gradient_offset += buffers.vertices.len();
                        num_gradients += 1;
                    }
                }
                // Its indices stay in the buffer too: skip them, or every
                // later mesh draws with another mesh's indices.
                index_offset += mesh.indices().len();
                continue;
            };

            render_pass.set_scissor_rect(
                clip_bounds.x,
                clip_bounds.y,
                clip_bounds.width,
                clip_bounds.height,
            );

            match mesh {
                Mesh::Solid { buffers, .. } => {
                    if !last_is_solid.unwrap_or(false) {
                        render_pass.set_pipeline(&solid.pipeline);

                        last_is_solid = Some(true);
                    }

                    render_pass.set_bind_group(
                        0,
                        &self.solid.constants,
                        &[(num_solids * std::mem::size_of::<Uniforms>())
                            as u32],
                    );

                    render_pass.set_vertex_buffer(
                        0,
                        self.solid.vertices.range(
                            solid_offset,
                            solid_offset + buffers.vertices.len(),
                        ),
                    );

                    num_solids += 1;
                    solid_offset += buffers.vertices.len();
                }
                Mesh::Gradient { buffers, .. } => {
                    if last_is_solid.unwrap_or(true) {
                        render_pass.set_pipeline(&gradient.pipeline);

                        last_is_solid = Some(false);
                    }

                    render_pass.set_bind_group(
                        0,
                        &self.gradient.constants,
                        &[(num_gradients * std::mem::size_of::<Uniforms>())
                            as u32],
                    );

                    render_pass.set_vertex_buffer(
                        0,
                        self.gradient.vertices.range(
                            gradient_offset,
                            gradient_offset + buffers.vertices.len(),
                        ),
                    );

                    num_gradients += 1;
                    gradient_offset += buffers.vertices.len();
                }
            };

            render_pass.set_index_buffer(
                self.index_buffer
                    .range(index_offset, index_offset + mesh.indices().len()),
                wgpu::IndexFormat::Uint32,
            );

            render_pass.draw_indexed(0..mesh.indices().len() as u32, 0, 0..1);

            index_offset += mesh.indices().len();
        }
    }
}

fn fragment_target(
    texture_format: wgpu::TextureFormat,
) -> wgpu::ColorTargetState {
    wgpu::ColorTargetState {
        format: texture_format,
        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        write_mask: wgpu::ColorWrites::ALL,
    }
}

fn primitive_state() -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        front_face: wgpu::FrontFace::Cw,
        ..Default::default()
    }
}

fn multisample_state(
    antialiasing: Option<Antialiasing>,
) -> wgpu::MultisampleState {
    wgpu::MultisampleState {
        count: antialiasing.map(Antialiasing::sample_count).unwrap_or(1),
        mask: !0,
        alpha_to_coverage_enabled: false,
    }
}

#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Uniforms {
    transform: [f32; 16],
    /// Uniform values must be 256-aligned;
    /// see: [`wgpu::Limits`] `min_uniform_buffer_offset_alignment`.
    _padding: [f32; 48],
}

impl Uniforms {
    pub fn new(transform: Transformation) -> Self {
        Self {
            transform: transform.into(),
            _padding: [0.0; 48],
        }
    }

    pub fn entry() -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(
                    std::mem::size_of::<Self>() as u64,
                ),
            },
            count: None,
        }
    }

    pub fn min_size() -> Option<wgpu::BufferSize> {
        wgpu::BufferSize::new(std::mem::size_of::<Self>() as u64)
    }
}

mod solid {
    use crate::Buffer;
    use crate::graphics::Antialiasing;
    use crate::graphics::mesh;
    use crate::triangle;

    #[derive(Debug, Clone)]
    pub struct Pipeline {
        pub pipeline: wgpu::RenderPipeline,
        pub constants_layout: wgpu::BindGroupLayout,
    }

    #[derive(Debug)]
    pub struct Layer {
        pub vertices: Buffer<mesh::SolidVertex2D>,
        pub uniforms: Buffer<triangle::Uniforms>,
        pub constants: wgpu::BindGroup,
    }

    impl Layer {
        pub fn new(
            device: &wgpu::Device,
            constants_layout: &wgpu::BindGroupLayout,
        ) -> Self {
            let vertices = Buffer::new(
                device,
                "iced_wgpu.triangle.solid.vertex_buffer",
                triangle::INITIAL_VERTEX_COUNT,
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            );

            let uniforms = Buffer::new(
                device,
                "iced_wgpu.triangle.solid.uniforms",
                1,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            );

            let constants =
                Self::bind_group(device, &uniforms.raw, constants_layout);

            Self {
                vertices,
                uniforms,
                constants,
            }
        }

        pub fn bind_group(
            device: &wgpu::Device,
            buffer: &wgpu::Buffer,
            layout: &wgpu::BindGroupLayout,
        ) -> wgpu::BindGroup {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("iced_wgpu.triangle.solid.bind_group"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(
                        wgpu::BufferBinding {
                            buffer,
                            offset: 0,
                            size: triangle::Uniforms::min_size(),
                        },
                    ),
                }],
            })
        }
    }

    impl Pipeline {
        pub fn new(
            device: &wgpu::Device,
            format: wgpu::TextureFormat,
            antialiasing: Option<Antialiasing>,
        ) -> Self {
            let constants_layout = device.create_bind_group_layout(
                &wgpu::BindGroupLayoutDescriptor {
                    label: Some("iced_wgpu.triangle.solid.bind_group_layout"),
                    entries: &[triangle::Uniforms::entry()],
                },
            );

            let layout = device.create_pipeline_layout(
                &wgpu::PipelineLayoutDescriptor {
                    label: Some("iced_wgpu.triangle.solid.pipeline_layout"),
                    bind_group_layouts: &[&constants_layout],
                    immediate_size: 0,
                },
            );

            let shader =
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("iced_wgpu.triangle.solid.shader"),
                    source: wgpu::ShaderSource::Wgsl(
                        std::borrow::Cow::Borrowed(concat!(
                            include_str!("shader/triangle.wgsl"),
                            "\n",
                            include_str!("shader/triangle/solid.wgsl"),
                            "\n",
                            include_str!("shader/color.wgsl"),
                        )),
                    ),
                });

            let pipeline =
                device.create_render_pipeline(
                    &wgpu::RenderPipelineDescriptor {
                        label: Some("iced_wgpu::triangle::solid pipeline"),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &shader,
                            entry_point: Some("solid_vs_main"),
                            buffers: &[wgpu::VertexBufferLayout {
                                array_stride: std::mem::size_of::<
                                    mesh::SolidVertex2D,
                                >(
                                )
                                    as u64,
                                step_mode: wgpu::VertexStepMode::Vertex,
                                attributes: &wgpu::vertex_attr_array!(
                                    // Position
                                    0 => Float32x2,
                                    // Color
                                    1 => Float32x4,
                                ),
                            }],
                            compilation_options:
                                wgpu::PipelineCompilationOptions::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &shader,
                            entry_point: Some("solid_fs_main"),
                            targets: &[Some(triangle::fragment_target(format))],
                            compilation_options:
                                wgpu::PipelineCompilationOptions::default(),
                        }),
                        primitive: triangle::primitive_state(),
                        depth_stencil: None,
                        multisample: triangle::multisample_state(antialiasing),
                        multiview_mask: None,
                        cache: None,
                    },
                );

            Self {
                pipeline,
                constants_layout,
            }
        }
    }
}

mod gradient {
    use crate::Buffer;
    use crate::graphics::Antialiasing;
    use crate::graphics::mesh;
    use crate::triangle;

    #[derive(Debug, Clone)]
    pub struct Pipeline {
        pub pipeline: wgpu::RenderPipeline,
        pub constants_layout: wgpu::BindGroupLayout,
    }

    #[derive(Debug)]
    pub struct Layer {
        pub vertices: Buffer<mesh::GradientVertex2D>,
        pub uniforms: Buffer<triangle::Uniforms>,
        pub constants: wgpu::BindGroup,
    }

    impl Layer {
        pub fn new(
            device: &wgpu::Device,
            constants_layout: &wgpu::BindGroupLayout,
        ) -> Self {
            let vertices = Buffer::new(
                device,
                "iced_wgpu.triangle.gradient.vertex_buffer",
                triangle::INITIAL_VERTEX_COUNT,
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            );

            let uniforms = Buffer::new(
                device,
                "iced_wgpu.triangle.gradient.uniforms",
                1,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            );

            let constants =
                Self::bind_group(device, &uniforms.raw, constants_layout);

            Self {
                vertices,
                uniforms,
                constants,
            }
        }

        pub fn bind_group(
            device: &wgpu::Device,
            uniform_buffer: &wgpu::Buffer,
            layout: &wgpu::BindGroupLayout,
        ) -> wgpu::BindGroup {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("iced_wgpu.triangle.gradient.bind_group"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(
                        wgpu::BufferBinding {
                            buffer: uniform_buffer,
                            offset: 0,
                            size: triangle::Uniforms::min_size(),
                        },
                    ),
                }],
            })
        }
    }

    impl Pipeline {
        pub fn new(
            device: &wgpu::Device,
            format: wgpu::TextureFormat,
            antialiasing: Option<Antialiasing>,
        ) -> Self {
            let constants_layout = device.create_bind_group_layout(
                &wgpu::BindGroupLayoutDescriptor {
                    label: Some(
                        "iced_wgpu.triangle.gradient.bind_group_layout",
                    ),
                    entries: &[triangle::Uniforms::entry()],
                },
            );

            let layout = device.create_pipeline_layout(
                &wgpu::PipelineLayoutDescriptor {
                    label: Some("iced_wgpu.triangle.gradient.pipeline_layout"),
                    bind_group_layouts: &[&constants_layout],
                    immediate_size: 0,
                },
            );

            let shader =
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("iced_wgpu.triangle.gradient.shader"),
                    source: wgpu::ShaderSource::Wgsl(
                        std::borrow::Cow::Borrowed(concat!(
                            include_str!("shader/triangle.wgsl"),
                            "\n",
                            include_str!("shader/triangle/gradient.wgsl"),
                            "\n",
                            include_str!("shader/color.wgsl"),
                            "\n",
                            include_str!("shader/color/linear_rgb.wgsl")
                        )),
                    ),
                });

            let pipeline = device.create_render_pipeline(
                &wgpu::RenderPipelineDescriptor {
                    label: Some("iced_wgpu.triangle.gradient.pipeline"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("gradient_vs_main"),
                        buffers: &[wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<
                                mesh::GradientVertex2D,
                            >()
                                as u64,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &wgpu::vertex_attr_array!(
                                // Position
                                0 => Float32x2,
                                // Colors 1-2
                                1 => Uint32x4,
                                // Colors 3-4
                                2 => Uint32x4,
                                // Colors 5-6
                                3 => Uint32x4,
                                // Colors 7-8
                                4 => Uint32x4,
                                // Offsets
                                5 => Uint32x4,
                                // Direction
                                6 => Float32x4
                            ),
                        }],
                        compilation_options:
                            wgpu::PipelineCompilationOptions::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("gradient_fs_main"),
                        targets: &[Some(triangle::fragment_target(format))],
                        compilation_options:
                            wgpu::PipelineCompilationOptions::default(),
                    }),
                    primitive: triangle::primitive_state(),
                    depth_stencil: None,
                    multisample: triangle::multisample_state(antialiasing),
                    multiview_mask: None,
                    cache: None,
                },
            );

            Self {
                pipeline,
                constants_layout,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{Color, Font, Pixels, Rectangle, Renderer as _, Size};
    use crate::graphics::mesh::{self, Indexed, Mesh, SolidVertex2D};
    use crate::graphics::{Antialiasing, Shell, Viewport, color};
    use crate::{Engine, Renderer};

    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    const SIZE: u32 = 64;

    /// Native wgpu resolves these futures when they are first polled.
    fn ready<T>(future: impl Future<Output = T>) -> T {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());

        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    /// A renderer on the OpenGL backend with multisampled meshes, as a
    /// machine without a working Vulkan driver gets, or `None` without one.
    fn gl_renderer() -> Option<Renderer> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::GL,
            ..Default::default()
        });
        let adapter = ready(instance.request_adapter(
            &wgpu::RequestAdapterOptions::default(),
        ))
        .ok()?;
        let (device, queue) =
            ready(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .ok()?;
        let engine = Engine::new(
            &adapter,
            device,
            queue,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            Some(Antialiasing::MSAAx4),
            Shell::headless(),
        );

        Some(Renderer::new(engine, Font::DEFAULT, Pixels(16.0)))
    }

    fn square(bounds: Rectangle, color: Color) -> Mesh {
        let corner = |x: f32, y: f32| SolidVertex2D {
            position: [x, y],
            color: color::pack(color),
        };
        let (left, top) = (bounds.x, bounds.y);
        let (right, bottom) = (left + bounds.width, top + bounds.height);

        Mesh::Solid {
            buffers: Indexed {
                vertices: vec![
                    corner(left, top),
                    corner(right, top),
                    corner(right, bottom),
                    corner(left, bottom),
                ],
                indices: vec![0, 1, 2, 0, 2, 3],
            },
            transformation: crate::core::Transformation::IDENTITY,
            clip_bounds: bounds,
        }
    }

    fn pixel(frame: &[u8], x: u32, y: u32) -> [u8; 4] {
        let at = ((y * SIZE + x) * 4) as usize;

        frame[at..at + 4].try_into().expect("an RGBA pixel")
    }

    #[test]
    fn a_mesh_is_not_drawn_again_after_it_is_gone() {
        let Some(mut renderer) = gl_renderer() else {
            eprintln!("skipped: no OpenGL adapter to render with");
            return;
        };
        let viewport =
            Viewport::with_physical_size(Size::new(SIZE, SIZE), 1.0);
        let whole = Rectangle::with_size(Size::new(SIZE as f32, SIZE as f32));
        let red = Color::from_rgb(1.0, 0.0, 0.0);

        // The first frame draws a square in the top left corner.
        renderer.reset(whole);
        mesh::Renderer::draw_mesh(
            &mut renderer,
            square(Rectangle::new([4.0, 4.0].into(), [16.0, 16.0].into()), red),
        );
        let first = renderer.screenshot(&viewport, Color::BLACK);
        assert_eq!(pixel(&first, 12, 12), [255, 0, 0, 255]);

        // The next draws only one in a layer of its own at the bottom right.
        renderer.reset(whole);
        let corner = Rectangle::new([40.0, 40.0].into(), [20.0, 20.0].into());
        renderer.start_layer(corner);
        mesh::Renderer::draw_mesh(
            &mut renderer,
            square(Rectangle::new([44.0, 44.0].into(), [12.0, 12.0].into()), red),
        );
        renderer.end_layer();
        let next = renderer.screenshot(&viewport, Color::BLACK);

        assert_eq!(pixel(&next, 50, 50), [255, 0, 0, 255]);
        assert_eq!(
            pixel(&next, 12, 12),
            [0, 0, 0, 255],
            "the first frame's square is drawn again where no mesh is"
        );
    }
    #[test]
    fn every_mesh_in_a_layer_is_drawn_not_only_the_last() {
        let Some(mut renderer) = gl_renderer() else {
            eprintln!("skipped: no OpenGL adapter to render with");
            return;
        };
        let viewport =
            Viewport::with_physical_size(Size::new(SIZE, SIZE), 1.0);
        let whole = Rectangle::with_size(Size::new(SIZE as f32, SIZE as f32));
        let red = Color::from_rgb(1.0, 0.0, 0.0);

        // Each mesh is clipped to its own square, so the last one leaves its
        // clip set when the pass ends.
        renderer.reset(whole);
        mesh::Renderer::draw_mesh(
            &mut renderer,
            square(Rectangle::new([4.0, 4.0].into(), [16.0, 16.0].into()), red),
        );
        mesh::Renderer::draw_mesh(
            &mut renderer,
            square(Rectangle::new([44.0, 44.0].into(), [12.0, 12.0].into()), red),
        );
        let frame = renderer.screenshot(&viewport, Color::BLACK);

        assert_eq!(pixel(&frame, 50, 50), [255, 0, 0, 255]);
        assert_eq!(
            pixel(&frame, 12, 12),
            [255, 0, 0, 255],
            "only the last mesh of the layer is drawn"
        );
    }
}
