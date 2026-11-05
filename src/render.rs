//! The render thread: owns the wgpu device and draws the viewport.
//!
//! Nothing here touches GPUI. The UI thread sends [`Cmd`]s and receives
//! [`UiEvent`]s, and neither thread waits on the other, so a frame that takes
//! a second to draw never delays a GPUI frame, and the viewport keeps its own
//! frame rate, down to none at all when nothing changes.

use std::num::NonZeroU64;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytemuck::{Pod, Zeroable};
use futures::channel::mpsc::UnboundedSender;
use glam::{Mat4, Vec3};

use crate::camera::{Camera, Glide, Projection};
use crate::scene::{Model, Vertex};

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const PICK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
const SAMPLES: u32 = 4;
const TRIAD_SIZE: f32 = 110.0;
/// Bits of a part id holding the part's index in its assembly.
const PART_BITS: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayMode {
    Shaded = 0,
    ShadedEdges = 1,
    HiddenLine = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewPreset {
    Iso,
    Top,
    Front,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Section {
    /// 0, 1 or 2 for the plane normal to X, Y or Z; `None` for no section.
    pub axis: Option<usize>,
    /// Where the plane sits across the model, from 0 to 1.
    pub position: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub enum PointerKind {
    Down(Button),
    Up(Button),
    Move,
    /// Lines scrolled, positive away from the user.
    Scroll(f32),
    Leave,
}

#[derive(Clone, Copy, Debug)]
pub struct Pointer {
    pub kind: PointerKind,
    /// In the viewport's device pixels.
    pub x: f32,
    pub y: f32,
    /// In the window's logical pixels, for placing GPUI overlays.
    pub window_x: f32,
    pub window_y: f32,
    /// Shift held: dragging with the left button pans.
    pub shift: bool,
}

pub enum Cmd {
    Resize { width: u32, height: u32 },
    Pointer(Pointer),
    SetMode(DisplayMode),
    SetProjection(Projection),
    SetView(ViewPreset),
    Fit,
    SetSection(Section),
    SetArray(u32),
    SetSpin(bool),
    /// Selects a part of the first assembly; `None` clears the selection.
    SelectPart(Option<usize>),
    Shutdown,
}

/// A part of one of the assemblies in the array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartRef {
    pub assembly: u32,
    pub part: usize,
}

impl PartRef {
    fn from_id(id: u32) -> Option<Self> {
        (id != 0).then(|| {
            let raw = id - 1;
            Self {
                assembly: raw >> PART_BITS,
                part: (raw & ((1 << PART_BITS) - 1)) as usize,
            }
        })
    }

    fn id(self) -> u32 {
        (self.assembly << PART_BITS | self.part as u32) + 1
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Frames per second over the last reporting period; 0 when idle.
    pub fps: f32,
    /// CPU time to update and encode a frame, excluding waiting for vsync.
    pub encode_ms: f32,
    pub frames_total: u64,
    pub triangles: u64,
    pub instances: u32,
    pub draw_calls: u32,
    pub width: u32,
    pub height: u32,
}

pub enum UiEvent {
    /// The renderer has its first frame up.
    Ready,
    Failed(String),
    Stats(Stats),
    Hover {
        part: Option<PartRef>,
        window_x: f32,
        window_y: f32,
    },
    Selected(Option<PartRef>),
    ContextMenu {
        window_x: f32,
        window_y: f32,
    },
    /// The viewport was pressed, so GPUI should dismiss what is open.
    ViewportPressed,
    /// The swap chain was configured and has now presented, so the composition
    /// has to be committed for it to appear.
    CommitComposition,
}

/// What must be created on the UI thread: the surface, whose first configure
/// attaches a swap chain to the DirectComposition visual GPUI owns.
pub struct Gpu {
    /// `None` when rendering off screen, in `--offscreen`.
    pub surface: Option<wgpu::Surface<'static>>,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
}

#[cfg(target_os = "windows")]
pub fn create_gpu(
    visual: &windows::Win32::Graphics::DirectComposition::IDCompositionVisual,
) -> Result<Gpu> {
    use windows::core::Interface as _;

    let instance = dx12_instance();
    // SAFETY: wgpu takes its own reference to the visual and holds it for as
    // long as the surface lives.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(visual.as_raw()))
    }
    .context("creating a wgpu surface on the composition visual")?;
    let adapter = gpui::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .context("requesting a DX12 adapter")?;
    let (device, queue) = gpui::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("cad_viewport"),
        required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
        ..Default::default()
    }))
    .context("creating the wgpu device")?;

    let capabilities = surface.get_capabilities(&adapter);
    let format = capabilities
        .formats
        .iter()
        .copied()
        .find(|format| *format == wgpu::TextureFormat::Bgra8UnormSrgb)
        .or_else(|| capabilities.formats.iter().copied().find(|format| format.has_srgb_suffix()))
        .context("the surface has no sRGB format")?;
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: 1,
        height: 1,
        // vsync blocks only the render thread.
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 1,
        alpha_mode: wgpu::CompositeAlphaMode::Opaque,
        color_space: wgpu::SurfaceColorSpace::Auto,
        view_formats: Vec::new(),
    };
    surface.configure(&device, &config);
    Ok(Gpu {
        surface: Some(surface),
        adapter,
        device,
        queue,
        config,
    })
}

fn dx12_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    })
}

pub fn adapter_summary(adapter: &wgpu::Adapter) -> String {
    let info = adapter.get_info();
    format!("{} ({:?})", info.name, info.backend)
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    forward: [f32; 4],
    section: [f32; 4],
    grid: [f32; 4],
    ids: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    model: [[f32; 4]; 4],
    color: [f32; 3],
    id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LineVertex {
    pos: [f32; 3],
    color: [f32; 4],
}

struct Draw {
    mesh: usize,
    first_instance: u32,
    instance_count: u32,
}

struct Pipelines {
    background: wgpu::RenderPipeline,
    solid: wgpu::RenderPipeline,
    edge: wgpu::RenderPipeline,
    grid: wgpu::RenderPipeline,
    triad: wgpu::RenderPipeline,
    pick: wgpu::RenderPipeline,
}

struct UniformBlock {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

struct Press {
    pan: bool,
    start: (f32, f32),
    last: (f32, f32),
    moved: bool,
}

struct Renderer {
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    ui: UnboundedSender<UiEvent>,

    pipelines: Pipelines,
    main_uniforms: UniformBlock,
    triad_uniforms: UniformBlock,
    pick_uniforms: UniformBlock,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    edge_buffer: wgpu::Buffer,
    triad_buffer: wgpu::Buffer,
    grid_buffer: Option<wgpu::Buffer>,
    grid_vertices: u32,
    grid_center: (f32, f32),
    grid_radius: f32,
    instance_buffer: Option<wgpu::Buffer>,
    msaa: Option<wgpu::TextureView>,
    depth: Option<wgpu::TextureView>,
    pick_color: wgpu::Texture,
    pick_depth: wgpu::TextureView,
    pick_readback: wgpu::Buffer,

    model: Arc<Model>,
    instances: Vec<Instance>,
    draws: Vec<Draw>,
    array: u32,
    bounds: (Vec3, Vec3),
    instances_dirty: bool,

    camera: Camera,
    glide: Option<Glide>,
    mode: DisplayMode,
    section: Section,
    spin: bool,
    model_time: f32,
    last_frame: Instant,
    dirty: bool,

    press: Option<Press>,
    hover_request: Option<(f32, f32, f32, f32)>,
    hovered: u32,
    selected: u32,
    announced_ready: bool,
    /// Set when the swap chain is configured, until a frame has been presented.
    commit_pending: bool,

    frames_total: u64,
    frames_since_report: u32,
    encode_seconds_since_report: f32,
    last_report: Instant,
    reported_idle: bool,
}

fn uniform_block(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    label: &str,
) -> UniformBlock {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size_of::<Globals>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    UniformBlock { buffer, bind_group }
}

fn static_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    bytes: &[u8],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.len().max(4) as u64,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, bytes);
    buffer
}

fn create_pipelines(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, format: wgpu::TextureFormat) -> Pipelines {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("cad_viewport"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("cad_viewport"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });

    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
    };
    let edge_layout = wgpu::VertexBufferLayout {
        array_stride: 12,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3],
    };
    let line_layout = wgpu::VertexBufferLayout {
        array_stride: size_of::<LineVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
    };
    let instance_layout = wgpu::VertexBufferLayout {
        array_stride: size_of::<Instance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![
            2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4,
            6 => Float32x3, 7 => Uint32
        ],
    };

    struct Desc<'a> {
        label: &'a str,
        vertex: &'a str,
        fragment: &'a str,
        buffers: &'a [Option<wgpu::VertexBufferLayout<'a>>],
        topology: wgpu::PrimitiveTopology,
        target: wgpu::TextureFormat,
        blend: Option<wgpu::BlendState>,
        depth_compare: wgpu::CompareFunction,
        depth_write: bool,
        bias: wgpu::DepthBiasState,
        samples: u32,
    }
    let build = |desc: Desc| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(desc.label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some(desc.vertex),
                compilation_options: Default::default(),
                buffers: desc.buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(desc.fragment),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: desc.target,
                    blend: desc.blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: desc.topology,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(desc.depth_write),
                depth_compare: Some(desc.depth_compare),
                stencil: wgpu::StencilState::default(),
                bias: desc.bias,
            }),
            multisample: wgpu::MultisampleState {
                count: desc.samples,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        })
    };

    Pipelines {
        background: build(Desc {
            label: "background",
            vertex: "vs_background",
            fragment: "fs_background",
            buffers: &[],
            topology: wgpu::PrimitiveTopology::TriangleList,
            target: format,
            blend: None,
            depth_compare: wgpu::CompareFunction::Always,
            depth_write: false,
            bias: Default::default(),
            samples: SAMPLES,
        }),
        solid: build(Desc {
            label: "solid",
            vertex: "vs_solid",
            fragment: "fs_solid",
            buffers: &[Some(vertex_layout.clone()), Some(instance_layout.clone())],
            topology: wgpu::PrimitiveTopology::TriangleList,
            target: format,
            blend: None,
            // Reversed Z: greater is nearer.
            depth_compare: wgpu::CompareFunction::Greater,
            depth_write: true,
            // Pushed back a little, so feature edges win the depth test.
            bias: wgpu::DepthBiasState {
                constant: -6,
                slope_scale: -1.5,
                clamp: 0.0,
            },
            samples: SAMPLES,
        }),
        edge: build(Desc {
            label: "edge",
            vertex: "vs_edge",
            fragment: "fs_edge",
            buffers: &[Some(edge_layout), Some(instance_layout.clone())],
            topology: wgpu::PrimitiveTopology::LineList,
            target: format,
            blend: None,
            depth_compare: wgpu::CompareFunction::GreaterEqual,
            depth_write: false,
            bias: Default::default(),
            samples: SAMPLES,
        }),
        grid: build(Desc {
            label: "grid",
            vertex: "vs_line",
            fragment: "fs_line",
            buffers: &[Some(line_layout.clone())],
            topology: wgpu::PrimitiveTopology::LineList,
            target: format,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            depth_compare: wgpu::CompareFunction::Greater,
            depth_write: false,
            bias: Default::default(),
            samples: SAMPLES,
        }),
        triad: build(Desc {
            label: "triad",
            vertex: "vs_line",
            fragment: "fs_line",
            buffers: &[Some(line_layout)],
            topology: wgpu::PrimitiveTopology::LineList,
            target: format,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            depth_compare: wgpu::CompareFunction::Always,
            depth_write: false,
            bias: Default::default(),
            samples: SAMPLES,
        }),
        pick: build(Desc {
            label: "pick",
            vertex: "vs_pick",
            fragment: "fs_pick",
            buffers: &[Some(vertex_layout), Some(instance_layout)],
            topology: wgpu::PrimitiveTopology::TriangleList,
            target: PICK_FORMAT,
            blend: None,
            depth_compare: wgpu::CompareFunction::Greater,
            depth_write: true,
            bias: Default::default(),
            samples: 1,
        }),
    }
}

fn triad_vertices() -> Vec<LineVertex> {
    let axis = |direction: [f32; 3], color: [f32; 4]| {
        [
            LineVertex { pos: [0.0; 3], color },
            LineVertex { pos: direction, color },
        ]
    };
    [
        axis([1.0, 0.0, 0.0], [0.90, 0.09, 0.07, 1.0]),
        axis([0.0, 1.0, 0.0], [0.15, 0.70, 0.12, 1.0]),
        axis([0.0, 0.0, 1.0], [0.10, 0.32, 0.95, 1.0]),
    ]
    .concat()
}

impl Renderer {
    fn new(gpu: Gpu, model: Arc<Model>, ui: UnboundedSender<UiEvent>, array: u32, spin: bool, mode: DisplayMode) -> Self {
        let Gpu {
            surface,
            device,
            queue,
            config,
            ..
        } = gpu;
        // wgpu's default handler panics, which would silently end this thread.
        device.on_uncaptured_error(Arc::new({
            let ui = ui.clone();
            move |error: wgpu::Error| {
                eprintln!("wgpu error: {error}");
                ui.unbounded_send(UiEvent::Failed(error.to_string())).ok();
            }
        }));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<Globals>() as u64),
                },
                count: None,
            }],
        });
        let pipelines = create_pipelines(&device, &layout, config.format);
        let main_uniforms = uniform_block(&device, &layout, "main_globals");
        let triad_uniforms = uniform_block(&device, &layout, "triad_globals");
        let pick_uniforms = uniform_block(&device, &layout, "pick_globals");

        let vertex_buffer = static_buffer(
            &device,
            &queue,
            "vertices",
            wgpu::BufferUsages::VERTEX,
            bytemuck::cast_slice(&model.vertices),
        );
        let index_buffer = static_buffer(
            &device,
            &queue,
            "indices",
            wgpu::BufferUsages::INDEX,
            bytemuck::cast_slice(&model.indices),
        );
        let edge_buffer = static_buffer(
            &device,
            &queue,
            "edges",
            wgpu::BufferUsages::VERTEX,
            bytemuck::cast_slice(&model.edge_vertices),
        );
        let triad_buffer = static_buffer(
            &device,
            &queue,
            "triad",
            wgpu::BufferUsages::VERTEX,
            bytemuck::cast_slice(&triad_vertices()),
        );

        let single_pixel = |format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pick"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let pick_color = single_pixel(
            PICK_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let pick_depth = single_pixel(DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT)
            .create_view(&Default::default());
        let pick_readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pick_readback"),
            size: u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let camera = Camera {
            target: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            distance: 1000.0,
            projection: Projection::Perspective,
        };
        let now = Instant::now();
        let mut renderer = Self {
            surface,
            device,
            queue,
            config,
            ui,
            pipelines,
            main_uniforms,
            triad_uniforms,
            pick_uniforms,
            vertex_buffer,
            index_buffer,
            edge_buffer,
            triad_buffer,
            grid_buffer: None,
            grid_vertices: 0,
            grid_center: (0.0, 0.0),
            grid_radius: 0.0,
            instance_buffer: None,
            msaa: None,
            depth: None,
            pick_color,
            pick_depth,
            pick_readback,
            model,
            instances: Vec::new(),
            draws: Vec::new(),
            array: 0,
            bounds: (Vec3::ZERO, Vec3::ZERO),
            instances_dirty: true,
            camera,
            glide: None,
            mode,
            section: Section {
                axis: None,
                position: 0.5,
            },
            spin,
            model_time: 0.0,
            last_frame: now,
            dirty: true,
            press: None,
            hover_request: None,
            hovered: 0,
            selected: 0,
            announced_ready: false,
            commit_pending: true,
            frames_total: 0,
            frames_since_report: 0,
            encode_seconds_since_report: 0.0,
            last_report: now,
            reported_idle: false,
        };
        renderer.set_array(array);
        renderer.camera = renderer.preset_camera(ViewPreset::Iso);
        renderer
    }

    fn scene_radius(&self) -> f32 {
        (self.bounds.1 - self.bounds.0).length() * 0.5
    }

    fn assembly_offset(&self, assembly: u32) -> Vec3 {
        let size = self.model.max - self.model.min;
        let spacing = Vec3::new(size.x * 1.25, size.y * 1.25, 0.0);
        let columns = self.array;
        let (row, column) = (assembly / columns, assembly % columns);
        let half = (columns as f32 - 1.0) * 0.5;
        Vec3::new((column as f32 - half) * spacing.x, (row as f32 - half) * spacing.y, 0.0)
    }

    fn set_array(&mut self, array: u32) {
        if array == self.array {
            return;
        }
        self.array = array.max(1);
        let assemblies = self.array * self.array;

        self.instances.clear();
        self.draws.clear();
        for mesh in 0..self.model.meshes.len() {
            let first_instance = self.instances.len() as u32;
            for assembly in 0..assemblies {
                for (index, part) in self.model.parts.iter().enumerate() {
                    if part.mesh == mesh {
                        self.instances.push(Instance {
                            model: Mat4::IDENTITY.to_cols_array_2d(),
                            color: part.color,
                            id: PartRef { assembly, part: index }.id(),
                        });
                    }
                }
            }
            self.draws.push(Draw {
                mesh,
                first_instance,
                instance_count: self.instances.len() as u32 - first_instance,
            });
        }
        self.instance_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (self.instances.len() * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.instances_dirty = true;

        let first = self.assembly_offset(0);
        let last = self.assembly_offset(assemblies - 1);
        self.bounds = (self.model.min + first, self.model.max + last);
        self.rebuild_grid();
        self.dirty = true;
    }

    fn rebuild_grid(&mut self) {
        let (min, max) = self.bounds;
        let center = (min + max) * 0.5;
        let radius = self.scene_radius() * 2.6;
        // About 25 lines either way, on a 1-2-5 spacing, every fifth brighter.
        let rough = radius / 25.0;
        let magnitude = 10f32.powf(rough.log10().floor());
        let minor = magnitude
            * match rough / magnitude {
                ratio if ratio <= 1.0 => 1.0,
                ratio if ratio <= 2.0 => 2.0,
                ratio if ratio <= 5.0 => 5.0,
                _ => 10.0,
            };
        let lines = (radius / minor).ceil() as i64;
        let (first_x, first_y) = ((center.x / minor).round() as i64, (center.y / minor).round() as i64);
        let (snap_x, snap_y) = (first_x as f32 * minor, first_y as f32 * minor);
        let z = min.z;
        let mut vertices = Vec::with_capacity((lines as usize * 2 + 1) * 4);
        for index in -lines..=lines {
            let offset = index as f32 * minor;
            let (x, y) = (snap_x + offset, snap_y + offset);
            let color = |line: i64| {
                if line.rem_euclid(5) == 0 {
                    [0.30, 0.36, 0.46, 0.8]
                } else {
                    [0.16, 0.20, 0.26, 0.55]
                }
            };
            let line_x = color(first_x + index);
            let line_y = color(first_y + index);
            let span = lines as f32 * minor;
            vertices.push(LineVertex { pos: [x, snap_y - span, z], color: line_x });
            vertices.push(LineVertex { pos: [x, snap_y + span, z], color: line_x });
            vertices.push(LineVertex { pos: [snap_x - span, y, z], color: line_y });
            vertices.push(LineVertex { pos: [snap_x + span, y, z], color: line_y });
        }
        self.grid_vertices = vertices.len() as u32;
        self.grid_center = (center.x, center.y);
        self.grid_radius = radius;
        self.grid_buffer = Some(static_buffer(
            &self.device,
            &self.queue,
            "grid",
            wgpu::BufferUsages::VERTEX,
            bytemuck::cast_slice(&vertices),
        ));
    }

    /// The pose of a preset, framing the whole model.
    fn preset_camera(&self, preset: ViewPreset) -> Camera {
        const DEGREES: f32 = std::f32::consts::PI / 180.0;
        let (yaw, pitch) = match preset {
            ViewPreset::Iso => (-58.0 * DEGREES, 26.0 * DEGREES),
            ViewPreset::Top => (-90.0 * DEGREES, 89.9 * DEGREES),
            ViewPreset::Front => (-90.0 * DEGREES, 0.0),
            ViewPreset::Right => (0.0, 0.0),
        };
        let mut camera = self.fitted_camera();
        camera.yaw = yaw;
        camera.pitch = pitch;
        camera
    }

    fn fitted_camera(&self) -> Camera {
        let aspect = self.aspect();
        let radius = self.scene_radius();
        let half_fov = 17.5f32.to_radians();
        // The narrower of the two view directions has to hold the sphere.
        let half = if aspect < 1.0 { (half_fov.tan() * aspect).atan() } else { half_fov };
        Camera {
            target: (self.bounds.0 + self.bounds.1) * 0.5,
            distance: radius / half.sin() * 0.92,
            ..self.camera
        }
    }

    fn aspect(&self) -> f32 {
        self.config.width.max(1) as f32 / self.config.height.max(1) as f32
    }

    fn ready(&self) -> bool {
        self.config.width > 1 && self.config.height > 1
    }

    fn wants_frame(&self) -> bool {
        self.ready() && (self.dirty || self.spin || self.glide.is_some())
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        let was_ready = self.ready();
        self.config.width = width;
        self.config.height = height;
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
            self.commit_pending = true;
        }
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let attachment = |label, format| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: SAMPLES,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        self.msaa = Some(attachment("msaa", self.config.format));
        self.depth = Some(attachment("depth", DEPTH_FORMAT));
        if !was_ready {
            // The first real size: frame the model for this aspect ratio.
            self.camera = self.preset_camera(ViewPreset::Iso);
        }
        self.dirty = true;
    }

    fn apply(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Resize { width, height } => self.resize(width, height),
            Cmd::Pointer(pointer) => self.pointer(pointer),
            Cmd::SetMode(mode) => {
                self.mode = mode;
                self.dirty = true;
            }
            Cmd::SetProjection(projection) => {
                self.camera.projection = projection;
                self.dirty = true;
            }
            Cmd::SetView(preset) => {
                let to = self.preset_camera(preset);
                self.glide = Some(Glide::new(self.camera, to));
            }
            Cmd::Fit => {
                let to = self.fitted_camera();
                self.glide = Some(Glide::new(self.camera, to));
            }
            Cmd::SetSection(section) => {
                self.section = section;
                self.dirty = true;
            }
            Cmd::SetArray(array) => {
                self.set_array(array);
                let to = self.fitted_camera();
                self.glide = Some(Glide::new(self.camera, to));
            }
            Cmd::SetSpin(spin) => {
                self.spin = spin;
                self.dirty = true;
                self.last_frame = Instant::now();
            }
            Cmd::SelectPart(part) => {
                self.selected = part.map_or(0, |part| PartRef { assembly: 0, part }.id());
                self.dirty = true;
            }
            Cmd::Shutdown => return false,
        }
        true
    }

    fn pointer(&mut self, pointer: Pointer) {
        let position = (pointer.x, pointer.y);
        match pointer.kind {
            PointerKind::Down(button) => {
                self.ui.unbounded_send(UiEvent::ViewportPressed).ok();
                self.press = Some(Press {
                    pan: button == Button::Middle || pointer.shift,
                    start: position,
                    last: position,
                    moved: false,
                });
                self.glide = None;
                self.hover_request = None;
            }
            PointerKind::Move => {
                let Some(press) = &mut self.press else {
                    self.hover_request = Some((pointer.x, pointer.y, pointer.window_x, pointer.window_y));
                    return;
                };
                let (dx, dy) = (position.0 - press.last.0, position.1 - press.last.1);
                press.last = position;
                let travelled =
                    (position.0 - press.start.0).abs() + (position.1 - press.start.1).abs();
                press.moved |= travelled > 3.0;
                // Left and right both orbit; a right click that doesn't move
                // opens the menu instead.
                if press.pan {
                    self.camera.pan(dx, dy, self.config.height as f32);
                } else {
                    self.camera.orbit(dx, dy);
                }
                self.dirty = true;
            }
            PointerKind::Up(button) => {
                let Some(press) = self.press.take() else {
                    return;
                };
                if press.moved {
                    return;
                }
                match button {
                    Button::Left => {
                        let id = self.pick(pointer.x, pointer.y);
                        self.selected = id;
                        self.dirty = true;
                        self.ui
                            .unbounded_send(UiEvent::Selected(PartRef::from_id(id)))
                            .ok();
                    }
                    Button::Right => {
                        self.ui
                            .unbounded_send(UiEvent::ContextMenu {
                                window_x: pointer.window_x,
                                window_y: pointer.window_y,
                            })
                            .ok();
                    }
                    Button::Middle => {}
                }
            }
            PointerKind::Scroll(lines) => {
                let width = self.config.width.max(1) as f32;
                let height = self.config.height.max(1) as f32;
                let ndc = (pointer.x / width * 2.0 - 1.0, 1.0 - pointer.y / height * 2.0);
                self.glide = None;
                let aspect = self.aspect();
                self.camera.zoom_at((-lines * 0.12).exp(), ndc, aspect);
                self.dirty = true;
            }
            PointerKind::Leave => {
                self.hover_request = None;
                self.set_hovered(0, pointer.window_x, pointer.window_y);
            }
        }
    }

    fn set_hovered(&mut self, id: u32, window_x: f32, window_y: f32) {
        let changed = id != self.hovered;
        if changed {
            self.hovered = id;
            self.dirty = true;
        }
        if changed || id != 0 {
            self.ui
                .unbounded_send(UiEvent::Hover {
                    part: PartRef::from_id(id),
                    window_x,
                    window_y,
                })
                .ok();
        }
    }

    fn view_proj(&self) -> Mat4 {
        self.camera.projection_matrix(self.aspect(), self.scene_radius()) * self.camera.view()
    }

    fn globals(&self, view_proj: Mat4) -> Globals {
        let eye = self.camera.eye();
        let forward = self.camera.forward();
        let orthographic = self.camera.projection == Projection::Orthographic;
        let section = self.section.axis.map_or([0.0; 4], |axis| {
            let mut normal = [0.0; 3];
            normal[axis] = 1.0;
            let (min, max) = (self.bounds.0[axis], self.bounds.1[axis]);
            let distance = min + (max - min) * self.section.position;
            [normal[0], normal[1], normal[2], distance]
        });
        Globals {
            view_proj: view_proj.to_cols_array_2d(),
            eye: [eye.x, eye.y, eye.z, 1.0],
            forward: [forward.x, forward.y, forward.z, if orthographic { 1.0 } else { 0.0 }],
            section,
            grid: [self.grid_radius, self.grid_center.0, self.grid_center.1, 0.0],
            ids: [
                self.hovered,
                self.selected,
                self.mode as u32,
                u32::from(self.section.axis.is_some()),
            ],
        }
    }

    fn update_instances(&mut self) {
        let assemblies = self.array * self.array;
        let offsets: Vec<Vec3> = (0..assemblies).map(|index| self.assembly_offset(index)).collect();
        let time = self.model_time;
        let transforms: Vec<Mat4> = self.model.parts.iter().map(|part| part.transform(time)).collect();
        for instance in &mut self.instances {
            let part = PartRef::from_id(instance.id).expect("instances have ids");
            let transform = Mat4::from_translation(offsets[part.assembly as usize]) * transforms[part.part];
            instance.model = transform.to_cols_array_2d();
        }
        let buffer = self.instance_buffer.as_ref().expect("allocated with the array");
        self.queue.write_buffer(buffer, 0, bytemuck::cast_slice(&self.instances));
        self.instances_dirty = false;
    }

    /// Moves the model and the camera on to now.
    fn advance(&mut self) -> Instant {
        let started = Instant::now();
        let dt = (started - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = started;
        if self.spin {
            self.model_time += dt;
            self.instances_dirty = true;
        }
        if let Some(glide) = &self.glide {
            let (camera, arrived) = glide.sample();
            self.camera = camera;
            if arrived {
                self.glide = None;
            }
        }
        if self.instances_dirty {
            self.update_instances();
        }
        started
    }

    fn frame(&mut self) {
        let started = self.advance();
        let Some(surface) = &self.surface else {
            return;
        };
        let surface_texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&self.device, &self.config);
                self.commit_pending = true;
                return;
            }
            _ => {
                // Occluded or timed out: stay dirty and try again shortly.
                std::thread::sleep(Duration::from_millis(16));
                return;
            }
        };
        let frame_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let commands = self.encode(&frame_view);
        self.queue.submit([commands]);
        let encode_seconds = started.elapsed().as_secs_f32();
        self.queue.present(surface_texture);
        // wgpu leaves committing to whoever owns the composition device, and
        // nothing else commits while the layout holds still. Without this the
        // viewport stays black until the window is resized.
        if std::mem::take(&mut self.commit_pending) {
            self.ui.unbounded_send(UiEvent::CommitComposition).ok();
        }

        self.dirty = false;
        self.frames_total += 1;
        self.frames_since_report += 1;
        self.encode_seconds_since_report += encode_seconds;
        if !self.announced_ready {
            self.announced_ready = true;
            self.ui.unbounded_send(UiEvent::Ready).ok();
        }
    }

    /// Records a frame that resolves into `frame_view`.
    fn encode(&self, frame_view: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let view_proj = self.view_proj();
        self.queue.write_buffer(
            &self.main_uniforms.buffer,
            0,
            bytemuck::bytes_of(&self.globals(view_proj)),
        );
        // The triad turns with the camera but neither moves nor zooms.
        let rotation = Mat4::from_mat3(glam::Mat3::from_mat4(self.camera.view()));
        let triad = Mat4::orthographic_rh(-1.25, 1.25, -1.25, 1.25, 4.0, -4.0) * rotation;
        let mut triad_globals = self.globals(triad);
        triad_globals.grid = [0.0; 4];
        self.queue.write_buffer(
            &self.triad_uniforms.buffer,
            0,
            bytemuck::bytes_of(&triad_globals),
        );

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewport"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.msaa.as_ref().expect("allocated on resize"),
                    depth_slice: None,
                    resolve_target: Some(frame_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: self.depth.as_ref().expect("allocated on resize"),
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_bind_group(0, &self.main_uniforms.bind_group, &[]);
            pass.set_pipeline(&self.pipelines.background);
            pass.draw(0..3, 0..1);

            let instances = self.instance_buffer.as_ref().expect("allocated with the array");
            pass.set_pipeline(&self.pipelines.solid);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_vertex_buffer(1, instances.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            for draw in &self.draws {
                let mesh = &self.model.meshes[draw.mesh];
                pass.draw_indexed(
                    mesh.first_index..mesh.first_index + mesh.index_count,
                    mesh.base_vertex,
                    draw.first_instance..draw.first_instance + draw.instance_count,
                );
            }

            if self.mode != DisplayMode::Shaded {
                pass.set_pipeline(&self.pipelines.edge);
                pass.set_vertex_buffer(0, self.edge_buffer.slice(..));
                for draw in &self.draws {
                    let mesh = &self.model.meshes[draw.mesh];
                    pass.draw(
                        mesh.first_edge..mesh.first_edge + mesh.edge_count,
                        draw.first_instance..draw.first_instance + draw.instance_count,
                    );
                }
            }

            if let Some(grid) = &self.grid_buffer {
                pass.set_pipeline(&self.pipelines.grid);
                pass.set_vertex_buffer(0, grid.slice(..));
                pass.draw(0..self.grid_vertices, 0..1);
            }

            let side = TRIAD_SIZE.min(self.config.width as f32).min(self.config.height as f32);
            pass.set_viewport(8.0, self.config.height as f32 - side - 8.0, side, side, 0.0, 1.0);
            pass.set_bind_group(0, &self.triad_uniforms.bind_group, &[]);
            pass.set_pipeline(&self.pipelines.triad);
            pass.set_vertex_buffer(0, self.triad_buffer.slice(..));
            pass.draw(0..6, 0..1);
        }
        encoder.finish()
    }

    /// Renders one pixel of part ids and reads it back.
    fn pick(&mut self, x: f32, y: f32) -> u32 {
        let (width, height) = (self.config.width.max(1) as f32, self.config.height.max(1) as f32);
        let ndc_x = 2.0 * (x.floor() + 0.5) / width - 1.0;
        let ndc_y = 1.0 - 2.0 * (y.floor() + 0.5) / height;
        // Blows the pixel under the cursor up to fill the single-pixel target.
        let pick_matrix = Mat4::from_scale(Vec3::new(width, height, 1.0))
            * Mat4::from_translation(Vec3::new(-ndc_x, -ndc_y, 0.0));
        let globals = self.globals(pick_matrix * self.view_proj());
        self.queue.write_buffer(&self.pick_uniforms.buffer, 0, bytemuck::bytes_of(&globals));
        if self.instances_dirty {
            self.update_instances();
        }

        let color_view = self.pick_color.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pick"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.pick_depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let instances = self.instance_buffer.as_ref().expect("allocated with the array");
            pass.set_pipeline(&self.pipelines.pick);
            pass.set_bind_group(0, &self.pick_uniforms.bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_vertex_buffer(1, instances.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            for draw in &self.draws {
                let mesh = &self.model.meshes[draw.mesh];
                pass.draw_indexed(
                    mesh.first_index..mesh.first_index + mesh.index_count,
                    mesh.base_vertex,
                    draw.first_instance..draw.first_instance + draw.instance_count,
                );
            }
        }
        encoder.copy_texture_to_buffer(
            self.pick_color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.pick_readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let submission = self.queue.submit([encoder.finish()]);
        let slice = self.pick_readback.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).ok();
        });
        let polled = self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(5)),
        });
        if polled.is_err() || !matches!(receiver.recv(), Ok(Ok(()))) {
            return 0;
        }
        let id = {
            let mapped = slice.get_mapped_range().expect("mapped for reading");
            u32::from_ne_bytes([mapped[0], mapped[1], mapped[2], mapped[3]])
        };
        self.pick_readback.unmap();
        id
    }

    /// Answers the latest hover once the queue is empty, so a stream of mouse
    /// moves costs one pick rather than one each.
    fn service_hover(&mut self) {
        if self.press.is_some() {
            return;
        }
        if let Some((x, y, window_x, window_y)) = self.hover_request.take() {
            let id = self.pick(x, y);
            self.set_hovered(id, window_x, window_y);
        }
    }

    fn report(&mut self, force: bool) {
        let elapsed = self.last_report.elapsed().as_secs_f32();
        if !force && elapsed < 0.25 {
            return;
        }
        let frames = self.frames_since_report.max(1) as f32;
        let assemblies = u64::from(self.array * self.array);
        self.ui
            .unbounded_send(UiEvent::Stats(Stats {
                fps: self.frames_since_report as f32 / elapsed.max(0.001),
                encode_ms: self.encode_seconds_since_report / frames * 1000.0,
                frames_total: self.frames_total,
                triangles: self.model.triangles_per_assembly() * assemblies,
                instances: self.instances.len() as u32,
                draw_calls: self.draws.len() as u32,
                width: self.config.width,
                height: self.config.height,
            }))
            .ok();
        self.frames_since_report = 0;
        self.encode_seconds_since_report = 0.0;
        self.last_report = Instant::now();
    }

    fn run(&mut self, commands: mpsc::Receiver<Cmd>) {
        loop {
            if !self.wants_frame() {
                // Nothing to draw: report once and sleep until told otherwise.
                if !self.reported_idle && self.ready() {
                    self.report(true);
                    self.reported_idle = true;
                }
                match commands.recv() {
                    Ok(cmd) => {
                        if !self.apply(cmd) {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
            while let Ok(cmd) = commands.try_recv() {
                if !self.apply(cmd) {
                    return;
                }
            }
            if self.wants_frame() {
                self.reported_idle = false;
                self.frame();
                self.report(false);
            }
            self.service_hover();
        }
    }
}

/// Checks the renderer without a window, which is all a process outside the
/// interactive desktop session can do: draws known views to PNG files, tests
/// picking and measures how long frames take with nothing limiting them.
pub fn run_offscreen(model: Arc<Model>, out_dir: &std::path::Path) -> Result<()> {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 800;
    std::fs::create_dir_all(out_dir)?;

    let instance = dx12_instance();
    let adapter = gpui::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .context("requesting a DX12 adapter")?;
    println!("adapter: {}", adapter_summary(&adapter));
    let (device, queue) = gpui::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("cad_viewport_offscreen"),
        required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
        ..Default::default()
    }))
    .context("creating the wgpu device")?;
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        width: 1,
        height: 1,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 1,
        alpha_mode: wgpu::CompositeAlphaMode::Opaque,
        color_space: wgpu::SurfaceColorSpace::Auto,
        view_formats: Vec::new(),
    };
    let gpu = Gpu {
        surface: None,
        adapter,
        device,
        queue,
        config,
    };

    let (ui, _events) = futures::channel::mpsc::unbounded();
    let started = Instant::now();
    let mut renderer = Renderer::new(gpu, model.clone(), ui, 1, false, DisplayMode::ShadedEdges);
    println!("pipelines compiled in {:?}", started.elapsed());
    renderer.resize(WIDTH, HEIGHT);

    let target = renderer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: renderer.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    // Clear it once before its first resolve. D3D12 wants a placed render target
    // to start with a clear, and without this an AMD RX 550 reads back only
    // black, however many frames resolve into it. DXGI creates the swap chain's
    // buffers itself, so the on-screen path has no such step.
    {
        let mut encoder = renderer.device.create_command_encoder(&Default::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("offscreen_first_clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        renderer.queue.submit([encoder.finish()]);
    }
    let stride = (WIDTH * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = renderer.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("offscreen_readback"),
        size: u64::from(stride) * u64::from(HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let capture = |renderer: &mut Renderer, name: &str| -> Result<()> {
        renderer.advance();
        let commands = renderer.encode(&view);
        let mut encoder = renderer.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(HEIGHT),
                },
            },
            target.size(),
        );
        let submission = renderer.queue.submit([commands, encoder.finish()]);
        let (sender, receiver) = mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).ok();
            });
        renderer
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(60)),
            })
            .context("waiting for the frame")?;
        receiver.recv()?.context("mapping the frame")?;
        let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        {
            let mapped = readback.slice(..).get_mapped_range().expect("mapped for reading");
            for row in mapped.chunks_exact(stride as usize) {
                for bgra in row[..WIDTH as usize * 4].chunks_exact(4) {
                    pixels.extend_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
                }
            }
        }
        readback.unmap();
        anyhow::ensure!(
            pixels.chunks_exact(4).any(|pixel| pixel[..3] != [0, 0, 0]),
            "{name} came out entirely black"
        );
        image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels)
            .context("sizing the frame")?
            .save(out_dir.join(format!("{name}.png")))?;
        println!("wrote {name}.png");
        Ok(())
    };

    capture(&mut renderer, "iso_edges")?;
    renderer.mode = DisplayMode::Shaded;
    capture(&mut renderer, "iso_shaded")?;
    renderer.mode = DisplayMode::HiddenLine;
    capture(&mut renderer, "iso_hidden_line")?;
    renderer.mode = DisplayMode::ShadedEdges;

    renderer.section = Section {
        axis: Some(0),
        position: 0.42,
    };
    capture(&mut renderer, "section_x")?;
    renderer.section.axis = None;

    // Each pick, against the iso view: the gearbox in the middle, nothing in
    // the corner.
    renderer.selected = renderer.pick(WIDTH as f32 * 0.5, HEIGHT as f32 * 0.5);
    let name_of = |id: u32| {
        PartRef::from_id(id).map_or("nothing".to_string(), |part| model.parts[part.part].name.clone())
    };
    println!("pick at center: {}", name_of(renderer.selected));
    println!("pick at corner: {}", name_of(renderer.pick(2.0, 2.0)));
    capture(&mut renderer, "iso_selected")?;
    renderer.selected = 0;

    // Gear teeth up close: cut through the first stage and look along the
    // shafts, at two moments, to see whether the teeth mesh as they turn.
    let (min_y, max_y) = (renderer.bounds.0.y, renderer.bounds.1.y);
    renderer.section = Section {
        axis: Some(1),
        position: (-30.0 - min_y) / (max_y - min_y),
    };
    renderer.camera = Camera {
        target: Vec3::new(-96.0, -30.0, 0.0),
        yaw: std::f32::consts::FRAC_PI_2,
        pitch: 0.0,
        distance: 170.0,
        projection: Projection::Orthographic,
    };
    renderer.instances_dirty = true;
    capture(&mut renderer, "mesh_t0")?;
    renderer.model_time = 1.7;
    renderer.instances_dirty = true;
    capture(&mut renderer, "mesh_t1")?;
    renderer.section.axis = None;
    renderer.model_time = 0.0;
    renderer.camera.projection = Projection::Perspective;

    // Unthrottled frame times, waiting for the GPU after each frame.
    for (array, frames) in [(1, 120), (3, 120), (10, 60), (30, 30)] {
        renderer.set_array(array);
        renderer.camera = renderer.preset_camera(ViewPreset::Iso);
        renderer.spin = true;
        let started = Instant::now();
        for _ in 0..frames {
            renderer.advance();
            let submission = renderer.queue.submit([renderer.encode(&view)]);
            renderer
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(Duration::from_secs(60)),
                })
                .context("waiting for a frame")?;
        }
        let per_frame = started.elapsed().as_secs_f32() / frames as f32;
        println!(
            "array {array}x{array}: {} triangles, {} instances: {:.1} ms per frame ({:.0} fps possible)",
            model.triangles_per_assembly() * u64::from(array * array),
            renderer.instances.len(),
            per_frame * 1000.0,
            1.0 / per_frame,
        );
        renderer.spin = false;
    }
    capture(&mut renderer, "array_30")?;
    Ok(())
}

pub struct RenderOptions {
    pub array: u32,
    pub spin: bool,
    pub mode: DisplayMode,
}

pub fn spawn(
    gpu: Gpu,
    model: Arc<Model>,
    commands: mpsc::Receiver<Cmd>,
    ui: UnboundedSender<UiEvent>,
    options: RenderOptions,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("wgpu-viewport".into())
        .spawn(move || {
            // Compiling pipelines happens here, off the UI thread.
            let mut renderer = Renderer::new(gpu, model, ui, options.array, options.spin, options.mode);
            renderer.run(commands);
        })
        .expect("spawning the render thread")
}
