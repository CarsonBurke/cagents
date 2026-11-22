//! Clears part of a target to a color: a render pass can only clear its
//! whole attachment, but a frame that redraws only its damaged regions must
//! reset just those before drawing their layers again.
use crate::core::Color;
use crate::graphics;

#[derive(Debug, Clone)]
pub struct Pipeline {
    raw: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

impl Pipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("iced_wgpu.clear.layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(COLOR_SIZE),
                    },
                    count: None,
                }],
            });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("iced_wgpu.clear.pipeline"),
                bind_group_layouts: &[&layout],
                immediate_size: 0,
            });

        let shader =
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("iced_wgpu.clear.shader"),
                source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(
                    include_str!("shader/clear.wgsl"),
                )),
            });

        let raw =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("iced_wgpu.clear.pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options:
                        wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    // No blending: the region takes the color as a clear
                    // would, whatever its alpha.
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options:
                        wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        Self { raw, layout }
    }
}

const COLOR_SIZE: u64 = std::mem::size_of::<[f32; 4]>() as u64;

/// A renderer's clear color, uploaded once per color.
#[derive(Debug)]
pub struct State {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    color: Option<[f32; 4]>,
}

impl State {
    pub fn new(device: &wgpu::Device, pipeline: &Pipeline) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("iced_wgpu.clear.color"),
            size: COLOR_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("iced_wgpu.clear.bind_group"),
            layout: &pipeline.layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        });

        Self {
            buffer,
            bind_group,
            color: None,
        }
    }

    /// Sets the color later clears fill with: the same linear components a
    /// render pass's clear would store.
    pub fn prepare(&mut self, queue: &wgpu::Queue, color: Color) {
        let color = graphics::color::pack(color).components();

        if self.color != Some(color) {
            queue.write_buffer(&self.buffer, 0, bytemuck::cast_slice(&color));
            self.color = Some(color);
        }
    }

    /// Fills the render pass's current scissor rectangle with the color.
    pub fn render<'a>(
        &'a self,
        pipeline: &'a Pipeline,
        render_pass: &mut wgpu::RenderPass<'a>,
    ) {
        render_pass.set_pipeline(&pipeline.raw);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}
