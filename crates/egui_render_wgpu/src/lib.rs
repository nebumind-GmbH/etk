use bytemuck::cast_slice;
use egui_backend::egui::plot::Text;
use std::borrow::Cow;
use std::ops::Deref;
use egui::{
    epaint::ImageDelta, util::IdTypeMap, ClippedPrimitive, Mesh, PaintCallback, PaintCallbackInfo,
    Rect, TextureId,
};
use egui_backend::egui;
use egui_backend::{EguiGfxData, GfxBackend, WindowBackend, RenderTargetRect};
use intmap::IntMap;
use std::{
    convert::TryInto,
    num::{NonZeroU32, NonZeroU64},
    sync::{Arc, Mutex},
};
use tracing::{debug, info};
pub use wgpu;
use wgpu::{
    Adapter, AddressMode, Backends, BindGroup, BindGroupDescriptor, BindGroupEntry,
    BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType,
    BlendComponent, BlendFactor, BlendOperation, BlendState, Buffer, BufferBinding,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    CommandEncoder, CommandEncoderDescriptor, Device, DeviceDescriptor, Extent3d, FilterMode,
    FragmentState, FrontFace, ImageCopyTexture, ImageDataLayout, IndexFormat, Instance, Limits,
    LoadOp, MultisampleState, Operations, Origin3d, PipelineLayoutDescriptor, PolygonMode,
    PowerPreference, PresentMode, PrimitiveState, PrimitiveTopology, Queue, RenderPass,
    RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor,
    RequestAdapterOptions, Sampler, SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, Surface, SurfaceConfiguration, SurfaceTexture, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType,
    TextureUsages, TextureView, TextureViewDescriptor, TextureViewDimension, VertexAttribute,
    VertexBufferLayout, VertexFormat, VertexState, VertexStepMode,
};

/// This provides a Gfx backend for egui by implementing the `crate::GfxBackend` trait.
/// can be used by egui applications which want to render some objects  in the background but don't want a full renderer.
/// If you are making your own wgpu integration, then you can reuse the `EguiPainter` instead which contains only egui render specific data.
pub struct WgpuBackend {
    /// wgpu instance
    pub instance: Arc<Instance>,
    /// wgpu adapter
    pub adapter: Arc<Adapter>,
    /// wgpu device.
    pub device: Arc<Device>,
    /// wgpu queue. if you have commands that you would like to submit, instead push them into `Self::command_encoders`
    pub queue: Arc<Queue>,
    /// contains egui specific wgpu data like textures or buffers or pipelines etc..
    painter: EguiPainter,
    /// this is the window surface
    surface: Option<Surface>,
    surface_formats_priority: Vec<TextureFormat>,
    /// this configuration will be updated everytime we get a resize event during the `prepare_frame` fn
    pub surface_config: SurfaceConfiguration,
    /// once we acquire a swapchain image (surface texture), we will put it here.
    surface_current_texture: Option<SurfaceTexture>,
    /// we create a view for the swapchain image ^^ and set it to this field during the `prepare_frame` fn.
    /// users can assume that it will *always* be available during the `UserApp::run` fn. but don't keep any references as
    /// it will be taken and submitted during the `present_frame` method after rendering is done.
    /// surface is always cleared by wgpu, so no need to wipe it again.
    pub surface_view: Option<TextureView>,
    /// this is where we store our command encoders. we will create one during the `prepare_frame` fn.
    /// users can just use this. or create new encoders, and push them into this vec.
    /// `wgpu::Queue::submit` is very expensive, so we will submit ALL command encoders at the same time during the `present_frame` method
    /// just before presenting the swapchain image (surface texture).
    pub command_encoders: Vec<CommandEncoder>,
    /// use an offscreen render target
    pub use_offscreen_render_target: bool,
    /// this is an offscreen texture used for rendering egui
    pub offscreen_render_target: Arc<Mutex<Option<RenderTarget>>>,
    /// this is the rect the offscreen render target texture is rendered to
    pub render_target_rect: Option<RenderTargetRect>,

    last_surface_width: u32,
    last_surface_height: u32,
}

pub struct RenderTargetSize {
    pub width: u32,
    pub height: u32,
}

pub struct Percent(f32);

pub struct RenderTargetFullscreenRect {
    pub margin_top: Percent,
    pub margin_bottom: Percent,
    pub margin_left: Percent,
    pub margin_right: Percent,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub tex_coords: [f32; 2],
}

#[cfg(target_arch = "wasm32")]
pub const RENDER_TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
#[cfg(not(target_arch = "wasm32"))]
pub const RENDER_TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

const RENDER_TARGET_RECT: RenderTargetFullscreenRect =
    RenderTargetFullscreenRect {
        margin_top: Percent(2.0),
        margin_bottom: Percent(5.0),
        margin_left: Percent(7.0),
        margin_right: Percent(44.0),
    };

pub const RENDER_TARGET_BINDGROUP_ENTRIES: [BindGroupLayoutEntry; 2] = [
    BindGroupLayoutEntry {
        binding: 0,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Sampler(SamplerBindingType::Filtering),
        count: None,
    },
    BindGroupLayoutEntry {
        binding: 1,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Texture {
            sample_type: TextureSampleType::Float { filterable: true },
            view_dimension: TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    },
];

#[derive(Debug)]
pub struct RenderTarget {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub texture_format: wgpu::TextureFormat,
    pub bind_group_entries: [BindGroupLayoutEntry; 2],
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

pub struct WgpuConfig {
    pub backends: Backends,
    pub power_preference: PowerPreference,
    pub device_descriptor: DeviceDescriptor<'static>,
    pub surface_formats_priority: Vec<TextureFormat>,
    pub surface_config: SurfaceConfiguration,
}

impl Default for WgpuConfig {
    fn default() -> Self {
        Self {
            backends: Backends::all(),
            power_preference: PowerPreference::default(),
            device_descriptor: DeviceDescriptor {
                label: Some("my wgpu device"),
                features: Default::default(),
                limits: Limits::downlevel_webgl2_defaults(),
            },
            surface_config: SurfaceConfiguration {
                usage: TextureUsages::RENDER_ATTACHMENT,
                #[cfg(target_arch = "wasm32")]
                format: TextureFormat::Rgba8UnormSrgb,
                #[cfg(not(target_arch = "wasm32"))]
                format: TextureFormat::Bgra8UnormSrgb,
                width: 0,
                height: 0,
                present_mode: PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
            },
            surface_formats_priority: vec![
                TextureFormat::Bgra8UnormSrgb,
                TextureFormat::Rgba8UnormSrgb,
            ],
        }
    }
}

impl WgpuBackend {

    pub async fn new_async<W: WindowBackend>(
        window_backend: &mut W,
        config: <Self as GfxBackend<W>>::Configuration,
        use_offscreen_render_target: bool,
    ) -> Self {
        let WgpuConfig {
            power_preference,
            device_descriptor,
            surface_formats_priority,
            mut surface_config,
            backends,
        } = config;

        debug!("using wgpu backends: {:?}", backends);
        let instance = Arc::new(Instance::new(backends));
        debug!("iterating over all adapters");

        #[cfg(not(target_arch = "wasm32"))]
        for adapter in instance.enumerate_adapters(Backends::all()) {
            debug!("adapter: {:#?}", adapter.get_info());
        }

        let mut surface = window_backend
            .get_window()
            .map(|w| unsafe { instance.create_surface(w) });

        info!("is surfaced created at startup?: {}", surface.is_some());

        debug!("using power preference: {:?}", config.power_preference);
        let adapter = Arc::new(
            instance
                .request_adapter(&RequestAdapterOptions {
                    power_preference: power_preference,
                    force_fallback_adapter: false,
                    compatible_surface: surface.as_ref(),
                })
                .await
                .expect("failed to get adapter"),
        );

        info!("chosen adapter details: {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(&device_descriptor, Default::default())
            .await
            .expect("failed to create wgpu device");

        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let framebuffer_size = window_backend.get_live_physical_size_framebuffer().unwrap();
        surface_config.width = framebuffer_size[0];
        surface_config.height = framebuffer_size[1];

        debug!("device features: {:#?}", device.features());
        debug!("device limits: {:#?}", device.limits());
        Self::reconfigure_surface(
            window_backend,
            &mut surface,
            &instance,
            &adapter,
            &device,
            &surface_formats_priority,
            &mut surface_config,
        );

        let mut render_target_rect = None;

        let offscreen_render_target =
            if use_offscreen_render_target {
                render_target_rect = Some(createRenderTargetRectFromScreenSize(
                    surface_config.width,
                    surface_config.height
                ));

                Arc::new(Mutex::new(create_offscreen_render_target(
                    &device,
                    render_target_rect.as_ref().unwrap().width,
                    render_target_rect.as_ref().unwrap().height,
                    "render target texture",
                )))
            } else {
                Arc::new(Mutex::new(None))
            };

        let painter = EguiPainter::new(&device, surface_config.format);

        Self {
            instance,
            adapter,
            device,
            queue,
            painter,
            surface,
            surface_formats_priority,
            surface_config,
            surface_view: None,
            surface_current_texture: None,
            command_encoders: Vec::new(),
            use_offscreen_render_target,
            offscreen_render_target,
            render_target_rect,
            last_surface_height: framebuffer_size[1],
            last_surface_width: framebuffer_size[0],
        }
    }

    pub fn resize_render_target<W: WindowBackend>(&mut self, width: u32, height: u32) {
        let do_resize =
            self.last_surface_height != height ||
            self.last_surface_width != width;

        if do_resize {
            self.last_surface_height = height;
            self.last_surface_width = width;

            if self.use_offscreen_render_target {
                let (width, height) = <WgpuBackend as GfxBackend<W>>::updateRenderTargetRect(
                    self,
                    width,
                    height,
                );

                let mut render_target =
                    self.offscreen_render_target.lock().unwrap();

                *render_target = create_offscreen_render_target(
                    &self.device,
                    width,
                    height,
                    "render target texture",
                );
            }
        }
    }

    /// This basically checks if the surface needs creating. and then if needed, creates surface if window exists.
    /// then, it does all the work of configuring the surface.
    /// this is used during resume events to create a surface.
    fn reconfigure_surface<W: WindowBackend>(
        window_backend: &mut W,
        surface: &mut Option<Surface>,
        instance: &Instance,
        adapter: &Adapter,
        device: &Device,
        surface_formats_priority: &[TextureFormat],
        surface_config: &mut SurfaceConfiguration,
    ) {
        if surface.is_some() {
            return;
        }
        if let Some(window) = window_backend.get_window() {
            *surface = Some(unsafe { instance.create_surface(window) });

            let supported_formats = surface.as_ref().unwrap().get_supported_formats(adapter);
            debug!("supported formats of the surface: {supported_formats:#?}");

            let mut compatible_format_found = false;
            for sfmt in surface_formats_priority.iter() {
                debug!("checking if {sfmt:?} is supported");
                if supported_formats.contains(sfmt) {
                    debug!("{sfmt:?} is supported. setting it as surface format");
                    surface_config.format = *sfmt;
                    compatible_format_found = true;
                    break;
                }
            }
            if !compatible_format_found {
                tracing::error!("could not find compatible surface format from user provided formats. using the first supported format instead");
                surface_config.format = supported_formats
                    .first()
                    .copied()
                    .expect("surface has zero supported texture formats");
            }
            let size = window_backend.get_live_physical_size_framebuffer().unwrap();
            surface_config.width = size[0];
            surface_config.height = size[1];

            surface.as_ref().unwrap().configure(device, surface_config);
        }
    }

    pub fn register_native_texture(
        &mut self,
        view: &wgpu::TextureView,
        texture_filter: wgpu::FilterMode,
    ) -> egui::TextureId {
        self.painter.register_native_texture(
            &self.device,
            view,
            texture_filter
        )
    }

    pub fn createRenderTargetRectFromScreenSize(&self, screen_width: u32, screen_height: u32) -> RenderTargetRect {
        createRenderTargetRectFromScreenSize(screen_width, screen_height)
    }
}

impl<W: WindowBackend> GfxBackend<W> for WgpuBackend {
    type Configuration = WgpuConfig;

    fn new(
        window_backend: &mut W,
        config: Self::Configuration,
        use_offscreen_render_target: bool,
    ) -> Self {
        pollster::block_on(Self::new_async(
            window_backend,
            config,
            use_offscreen_render_target
        ))
    }

    fn suspend(&mut self, _window_backend: &mut W) {
        self.surface = None;
        self.surface_current_texture = None;
        self.surface_view = None;
    }

    fn resume(&mut self, window_backend: &mut W) {
        Self::reconfigure_surface(
            window_backend,
            &mut self.surface,
            &self.instance,
            &self.adapter,
            &self.device,
            &self.surface_formats_priority,
            &mut self.surface_config,
        );
        self.painter
            .on_resume(&self.device, self.surface_config.format);

        self.resize_render_target::<W>(self.surface_config.width, self.surface_config.height);
    }

    fn prepare_frame(&mut self, framebuffer_size_update: bool, window_backend: &mut W) {
        if framebuffer_size_update {
            let size = window_backend.get_live_physical_size_framebuffer().unwrap();
            self.surface_config.width = size[0];
            self.surface_config.height = size[1];
            self.surface
                .as_ref()
                .unwrap()
                .configure(&self.device, &self.surface_config);

            self.resize_render_target::<W>(self.surface_config.width, self.surface_config.height);
        }

        assert!(self.surface_current_texture.is_none());
        assert!(self.surface_view.is_none());

        if let Some(surface) = self.surface.as_ref() {
            let current_surface_texture = surface.get_current_texture().unwrap_or_else(|e| {
                let phy_fb_size = window_backend.get_live_physical_size_framebuffer().unwrap();
                self.surface_config.width = phy_fb_size[0];
                self.surface_config.height = phy_fb_size[1];
                surface.configure(&self.device, &self.surface_config);
                surface.get_current_texture().expect(&format!(
                    "failed to get surface even after reconfiguration. {e}"
                ))
            });
            let surface_view = current_surface_texture
                .texture
                .create_view(&TextureViewDescriptor {
                    label: Some("surface view"),
                    format: Some(self.surface_config.format),
                    dimension: Some(TextureViewDimension::D2),
                    aspect: TextureAspect::All,
                    base_mip_level: 0,
                    mip_level_count: None,
                    base_array_layer: 0,
                    array_layer_count: None,
                });

            self.surface_view = Some(surface_view);
            self.surface_current_texture = Some(current_surface_texture);
        }
    }

    fn render(&mut self, egui_gfx_data: EguiGfxData) {
        let screen_size = if self.use_offscreen_render_target {
            [
                self.render_target_rect.as_ref().unwrap().width,
                self.render_target_rect.as_ref().unwrap().height,
            ]
        } else {
            [self.surface_config.width, self.surface_config.height]
        };

        self.painter.upload_egui_data(
            &self.device,
            &self.queue,
            egui_gfx_data,
            screen_size,
        );

        let mut render_pass_closure = |view| {
            let mut command_encoder =
                self
                    .device
                    .create_command_encoder(&CommandEncoderDescriptor {
                        label: Some("egui command encoder"),
                    });
            {
                let mut egui_pass = command_encoder.begin_render_pass(
                    &RenderPassDescriptor {
                        label: Some("egui render pass"),
                        color_attachments: &[Some(RenderPassColorAttachment {
                            view,
                            resolve_target: None,
                            ops: Operations {
                                load: wgpu::LoadOp::Load,
                                store: true,
                            },
                        })],
                        depth_stencil_attachment: None,
                    }
                );
                self.painter.draw_egui_with_renderpass(&mut egui_pass);
            }

            self.command_encoders.push(command_encoder);
        };

        {
            let guard =
                self
                    .offscreen_render_target
                    .lock()
                    .unwrap();

            if guard
                .deref()
                .is_none()
            {
                let view =
                    self
                        .surface_view
                        .as_ref()
                        .expect("failed ot get surface view for egui render pass creation");

                render_pass_closure(view);
            } else {
                let view =
                    &guard
                        .as_ref()
                        .unwrap()
                        .view;

                render_pass_closure(view);
            }
        }
    }

    fn present(&mut self, _window_backend: &mut W) {
        self.queue.submit(
            std::mem::take(&mut self.command_encoders)
                .into_iter()
                .map(|encoder| encoder.finish()),
        );

        let guard =
                self
                    .offscreen_render_target
                    .lock()
                    .unwrap();

        { // leaves None in self.surface_view
            self.surface_view
                .take()
                .expect("failed to get surface view to present");
        }
        // leaves None in self.surface_current_texture
        self.surface_current_texture
            .take()
            .expect("failed to surface texture to preset")
            .present();
    }

    fn is_rendering_to_offscreen_render_target(&self) -> bool {
        self.use_offscreen_render_target
    }

    fn updateRenderTargetRect(&mut self, screen_width: u32, screen_height: u32) -> (u32, u32) {
        self.render_target_rect = Some(createRenderTargetRectFromScreenSize(screen_width, screen_height));
        let rect = self.render_target_rect.as_ref().unwrap();
        (rect.width, rect.height)
    }

    fn mouse_pos_screen_to_render_target_space(&self, x: f32, y: f32) -> (f32, f32) {
        let (x, y) = if self.use_offscreen_render_target {
            let rect = self.render_target_rect.as_ref().unwrap();
            (x - rect.x as f32, y - rect.y as f32)
        } else {
            (x, y)
        };

        (x, y)
    }
}

pub fn create_offscreen_render_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &str,
) -> Option<RenderTarget> {

    let texture_format = RENDER_TARGET_FORMAT;
    let bind_group_entries = RENDER_TARGET_BINDGROUP_ENTRIES;

    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    let desc = wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: texture_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING,
    };
    let texture = device.create_texture(&desc);

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(
        &wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            compare: None,
            lod_min_clamp: -100.0,
            lod_max_clamp: 100.0,
            ..Default::default()
        }
    );

    let bind_group_layout = device.create_bind_group_layout(
        &wgpu::BindGroupLayoutDescriptor {
            entries: &bind_group_entries,
            label: Some("render_target_bind_group_layout"),
        }
    );
    let bind_group = device.create_bind_group(
        &wgpu::BindGroupDescriptor {
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
            label: Some("color_render_target.bind_group"),
        }
    );

    Some(RenderTarget{
        texture,
        view,
        sampler,
        texture_format,
        bind_group_entries,
        bind_group_layout,
        bind_group,
        width,
        height,
    })
}

pub fn createRenderTargetRectFromScreenSize(
    screen_width: u32,
    screen_height: u32
) -> RenderTargetRect {
    let width_percent = screen_width as f32 / 100.0;
    let height_percent = screen_height as f32 / 100.0;

    let x = width_percent * RENDER_TARGET_RECT.margin_left.0;
    let width =
        width_percent *
            (100.0
                - RENDER_TARGET_RECT.margin_left.0
                - RENDER_TARGET_RECT.margin_right.0
            );

    let y = height_percent * RENDER_TARGET_RECT.margin_top.0;
    let height =
        height_percent *
            (100.0
                - RENDER_TARGET_RECT.margin_top.0
                - RENDER_TARGET_RECT.margin_bottom.0
            );

    RenderTargetRect {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
        screen_width,
        screen_height,
    }
}

pub fn create_fullscreen_vertices() -> [Vertex; 4] {
    // clip space goes from -1.0 to 1.0 on each axis
    // so total length of an axis is 2.0
    // so one percent is 0.02
    let percent = 0.02f32;

    let top_left_x = -1.0 + percent * RENDER_TARGET_RECT.margin_left.0;
    let top_left_y = 1.0 - percent * RENDER_TARGET_RECT.margin_top.0;

    let bottom_left_x = top_left_x;
    let bottom_left_y = -1.0 + percent * RENDER_TARGET_RECT.margin_bottom.0;

    let top_right_x = 1.0 - percent * RENDER_TARGET_RECT.margin_right.0;
    let top_right_y = top_left_y;

    let bottom_right_x = top_right_x;
    let bottom_right_y = bottom_left_y;

    let vertices = [
        Vertex {
            position: [bottom_left_x, bottom_left_y, 0.0],
            tex_coords: [0.0, 1.0],
        },
        Vertex {
            position: [bottom_right_x, bottom_right_y, 0.0],
            tex_coords: [1.0, 1.0],
        },
        Vertex {
            position: [top_right_x, top_right_y, 0.0],
            tex_coords: [1.0, 0.0],
        },
        Vertex {
            position: [top_left_x, top_left_y, 0.0],
            tex_coords: [0.0, 0.0],
        },
    ];

    vertices
}

pub const EGUI_SHADER_SRC: &str = include_str!("../../../shaders/egui.wgsl");

type PrepareCallback = dyn Fn(&Device, &Queue, &mut IdTypeMap) + Sync + Send;
type RenderCallback =
    dyn for<'a, 'b> Fn(PaintCallbackInfo, &'a mut RenderPass<'b>, &'b IdTypeMap) + Sync + Send;

pub struct CallbackFn {
    pub prepare: Arc<PrepareCallback>,
    pub paint: Arc<RenderCallback>,
}

impl Default for CallbackFn {
    fn default() -> Self {
        CallbackFn {
            prepare: Arc::new(|_, _, _| ()),
            paint: Arc::new(|_, _, _| ()),
        }
    }
}

pub struct EguiPainter {
    /// current capacity of vertex buffer
    vb_len: usize,
    /// current capacity of index buffer
    ib_len: usize,
    /// vertex buffer
    vb: Buffer,
    /// index buffer
    ib: Buffer,
    /// Uniform buffer to store screen size in logical pixels
    screen_size_buffer: Buffer,
    /// bind group for the Uniform buffer using layout entry `SCREEN_SIZE_UNIFORM_BUFFER_BINDGROUP_ENTRY`
    screen_size_bind_group: BindGroup,
    /// this layout is reused by all egui textures.
    texture_bindgroup_layout: BindGroupLayout,
    /// used by pipeline create function
    screen_size_bindgroup_layout: BindGroupLayout,
    /// used to check if this matches the new surface after resume event. otherwise, recompile render pipeline
    surface_format: TextureFormat,
    /// egui render pipeline
    pipeline: RenderPipeline,
    /// linear sampler for egui textures that need to create bindgroups
    linear_sampler: Sampler,
    /// nearest sampler for egui textures (especially font texture) that need to create bindgroups for binding to egui pipelien
    nearest_sampler: Sampler,

    /// these are textures uploaded by egui. intmap is much faster than btree or hashmaps.
    /// maybe we can use a proper struct instead of tuple?
    managed_textures: IntMap<EguiTexture>,
    #[allow(unused)]
    user_textures: IntMap<EguiTexture>,
    next_user_texture_id: u64,
    /// textures to free
    delete_textures: Vec<TextureId>,
    draw_calls: Vec<EguiDrawCalls>,
    custom_data: IdTypeMap,
}

/// textures uploaded by egui are represented by this struct
pub struct EguiTexture {
    // None for User texture
    pub texture: Option<Texture>,
    // None for User texture
    pub view: Option<TextureView>,
    pub bindgroup: BindGroup,
}
/// DrawCalls list so that we can just get all the work done in the pre_render stage (upload egui data)
pub enum EguiDrawCalls {
    Mesh {
        clip_rect: [u32; 4],
        texture_id: TextureId,
        base_vertex: i32,
        index_start: u32,
        index_end: u32,
    },
    Callback {
        paint_callback_info: PaintCallbackInfo,
        clip_rect: [u32; 4],
        paint_callback: PaintCallback,
    },
}

impl EguiPainter {
    pub fn draw_egui_with_renderpass<'rpass>(&'rpass mut self, rpass: &mut RenderPass<'rpass>) {
        // rpass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
        rpass.set_pipeline(&self.pipeline);
        rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);

        rpass.set_vertex_buffer(0, self.vb.slice(..));
        rpass.set_index_buffer(self.ib.slice(..), IndexFormat::Uint32);

        for draw_call in self.draw_calls.iter() {
            match draw_call {
                &EguiDrawCalls::Mesh {
                    clip_rect,
                    texture_id,
                    base_vertex,
                    index_start,
                    index_end,
                } => {
                    let [x, y, width, height] = clip_rect;
                    rpass.set_scissor_rect(x, y, width, height);
                    // because webgl : Draw elements base vertex is not supported
                    // we can't use base_vertex argument of draw_indexed.
                    // We will make sure that bound vertex buffer starts from base_vertex at zero.
                    rpass.set_vertex_buffer(0, self.vb.slice(base_vertex as u64 * 20..));

                    match texture_id {
                        TextureId::Managed(key) => {
                            rpass.set_bind_group(
                                1,
                                &self
                                    .managed_textures
                                    .get(key)
                                    .expect("cannot find managed texture")
                                    .bindgroup,
                                &[],
                            );
                        }
                        TextureId::User(key) => {
                            rpass.set_bind_group(
                                1,
                                &self
                                    .user_textures
                                    .get(key)
                                    .expect("cannot find user texture")
                                    .bindgroup,
                                &[],
                            );
                        }
                    }
                    rpass.draw_indexed(index_start..index_end, 0, 0..1);
                }
                EguiDrawCalls::Callback {
                    clip_rect,
                    paint_callback,
                    paint_callback_info,
                } => {
                    let [x, y, width, height] = *clip_rect;
                    rpass.set_scissor_rect(x, y, width, height);
                    (paint_callback
                        .callback
                        .downcast_ref::<CallbackFn>()
                        .expect("failed to downcast Callbackfn")
                        .paint)(
                        PaintCallbackInfo {
                            viewport: paint_callback_info.viewport,
                            clip_rect: paint_callback_info.clip_rect,
                            pixels_per_point: paint_callback_info.pixels_per_point,
                            screen_size_px: paint_callback_info.screen_size_px,
                        },
                        rpass,
                        &self.custom_data,
                    );
                }
            }
        }
    }
    pub fn create_render_pipeline(
        dev: &Device,
        pipeline_surface_format: TextureFormat,
        screen_size_bindgroup_layout: &BindGroupLayout,
        texture_bindgroup_layout: &BindGroupLayout,
    ) -> RenderPipeline {
        assert!(
            pipeline_surface_format.describe().srgb,
            "egui wgpu only supports srgb compatible framebuffer"
        );
        // pipeline layout. screensize uniform buffer for vertex shader + texture and sampler for fragment shader
        let egui_pipeline_layout = dev.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("egui pipeline layout"),
            bind_group_layouts: &[screen_size_bindgroup_layout, texture_bindgroup_layout],
            push_constant_ranges: &[],
        });
        // shader from the wgsl source.
        let shader_module = dev.create_shader_module(ShaderModuleDescriptor {
            label: Some("egui shader src"),
            source: ShaderSource::Wgsl(EGUI_SHADER_SRC.into()),
        });
        // create pipeline using shaders + pipeline layout
        let egui_pipeline = dev.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("egui pipeline"),
            layout: Some(&egui_pipeline_layout),
            vertex: VertexState {
                module: &shader_module,
                entry_point: "vs_main",
                buffers: &VERTEX_BUFFER_LAYOUT,
            },
            primitive: EGUI_PIPELINE_PRIMITIVE_STATE,
            depth_stencil: None,
            // support multi sampling in future?
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader_module,
                entry_point: "fs_main",
                targets: &[Some(ColorTargetState {
                    format: pipeline_surface_format,
                    blend: Some(EGUI_PIPELINE_BLEND_STATE),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview: None,
        });
        egui_pipeline
    }

    pub fn new(dev: &Device, surface_format: TextureFormat) -> Self {
        // create uniform buffer for screen size
        let screen_size_buffer = dev.create_buffer(&BufferDescriptor {
            label: Some("screen size uniform buffer"),
            size: 16,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // create temporary layout to create screensize uniform buffer bindgroup
        let screen_size_bindgroup_layout =
            dev.create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("egui screen size bindgroup layout"),
                entries: &SCREEN_SIZE_UNIFORM_BUFFER_BINDGROUP_ENTRY,
            });
        // create texture bindgroup layout. all egui textures need to have a bindgroup with this layout to use
        // them in egui draw calls.
        let texture_bindgroup_layout = dev.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("egui texture bind group layout"),
            entries: &TEXTURE_BINDGROUP_ENTRIES,
        });
        // create screen size bind group with the above layout. store this permanently to bind before drawing egui.
        let screen_size_bind_group = dev.create_bind_group(&BindGroupDescriptor {
            label: Some("egui bindgroup"),
            layout: &screen_size_bindgroup_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: BindingResource::Buffer(BufferBinding {
                    buffer: &screen_size_buffer,
                    offset: 0,
                    size: None,
                }),
            }],
        });

        let pipeline = Self::create_render_pipeline(
            dev,
            surface_format,
            &screen_size_bindgroup_layout,
            &texture_bindgroup_layout,
        );
        // linear and nearest samplers for egui textures to use for creation of their bindgroups
        let linear_sampler = dev.create_sampler(&EGUI_LINEAR_SAMPLER_DESCRIPTOR);
        let nearest_sampler = dev.create_sampler(&EGUI_NEAREST_SAMPLER_DESCRIPTOR);

        // empty vertex and index buffers.
        let vb = dev.create_buffer(&BufferDescriptor {
            label: Some("egui vertex buffer"),
            size: 0,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ib = dev.create_buffer(&BufferDescriptor {
            label: Some("egui index buffer"),
            size: 0,
            usage: BufferUsages::INDEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            screen_size_buffer,
            pipeline,
            linear_sampler,
            nearest_sampler,
            managed_textures: Default::default(),
            vb,
            ib,
            screen_size_bind_group,
            texture_bindgroup_layout,
            vb_len: 0,
            ib_len: 0,
            delete_textures: Vec::new(),
            draw_calls: Vec::new(),
            custom_data: IdTypeMap::default(),
            user_textures: Default::default(),
            next_user_texture_id: 0,
            screen_size_bindgroup_layout,
            surface_format,
        }
    }

    fn on_resume(&mut self, dev: &Device, surface_format: TextureFormat) {
        if self.surface_format != surface_format {
            self.pipeline = Self::create_render_pipeline(
                dev,
                surface_format,
                &self.screen_size_bindgroup_layout,
                &self.texture_bindgroup_layout,
            );
        }
    }

    fn set_textures(
        &mut self,
        dev: &Device,
        queue: &Queue,
        textures_delta_set: Vec<(TextureId, ImageDelta)>,
    ) {
        for (tex_id, delta) in textures_delta_set {
            let width = delta.image.width() as u32;
            let height = delta.image.height() as u32;

            let size = Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };

            let data_color32 = match &delta.image {
                egui::ImageData::Color(color_image) => {
                    Cow::Borrowed(&color_image.pixels)
                },
                egui::ImageData::Font(font_image) => {
                    Cow::Owned(font_image.srgba_pixels(Some(1.0)).collect::<Vec<_>>())
                }
            };

            let data_bytes: &[u8] = bytemuck::cast_slice(data_color32.as_slice());

            match tex_id {
                egui::TextureId::Managed(tex_id) => {
                    if let Some(_) = delta.pos {
                    } else {
                        let mip_level_count = 1;
                        let new_texture = dev.create_texture(&TextureDescriptor {
                            label: None,
                            size,
                            mip_level_count,
                            sample_count: 1,
                            dimension: TextureDimension::D2,
                            format: TextureFormat::Rgba8UnormSrgb,
                            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                        });

                        queue.write_texture(
                            ImageCopyTexture {
                                texture: &new_texture,
                                mip_level: 0,
                                origin: Origin3d::default(),
                                aspect: TextureAspect::All,
                            },
                            data_bytes,
                            ImageDataLayout {
                                offset: 0,
                                bytes_per_row: Some(
                                    NonZeroU32::new(size.width as u32 * 4)
                                        .expect("texture bytes per row is zero"),
                                ),
                                rows_per_image: Some(
                                    NonZeroU32::new(size.height as u32)
                                        .expect("texture rows count is zero"),
                                ),
                            },
                            size,
                        );
                        let view = new_texture.create_view(&TextureViewDescriptor {
                            label: None,
                            format: Some(TextureFormat::Rgba8UnormSrgb),
                            dimension: Some(TextureViewDimension::D2),
                            aspect: TextureAspect::All,
                            base_mip_level: 0,
                            mip_level_count: None,
                            base_array_layer: 0,
                            array_layer_count: None,
                        });
                        let bindgroup = dev.create_bind_group(&BindGroupDescriptor {
                            label: None,
                            layout: &self.texture_bindgroup_layout,
                            entries: &[
                                BindGroupEntry {
                                    binding: 0,
                                    resource: BindingResource::Sampler(if tex_id == 0 {
                                        &self.nearest_sampler
                                    } else {
                                        match delta.options.magnification {
                                            egui::TextureFilter::Nearest => &self.nearest_sampler,
                                            egui::TextureFilter::Linear => &self.linear_sampler,
                                        }
                                    }),
                                },
                                BindGroupEntry {
                                    binding: 1,
                                    resource: BindingResource::TextureView(&view),
                                },
                            ],
                        });
                        self.managed_textures.insert(
                            tex_id,
                            EguiTexture {
                                texture: Some(new_texture),
                                view: Some(view),
                                bindgroup,
                            },
                        );
                    }
                }
                egui::TextureId::User(_) => todo!(),
            }
        }
    }
    pub fn upload_egui_data(
        &mut self,
        dev: &Device,
        queue: &Queue,
        EguiGfxData {
            meshes,
            textures_delta,
            screen_size_logical,
        }: EguiGfxData,
        screen_size_physical: [u32; 2],
    ) {
        let scale = screen_size_physical[0] as f32 / screen_size_logical[0];
        self.draw_calls.clear();
        // first deal with textures
        {
            // we need to delete textures in textures_delta.free AFTER the draw calls
            // so we store them in self.delete_textures.
            // otoh, the textures that were scheduled to be deleted previous frame, we will delete now

            let delete_textures = std::mem::replace(&mut self.delete_textures, textures_delta.free);
            // remove textures to be deleted in previous frame
            for tid in delete_textures {
                match tid {
                    TextureId::Managed(key) => {
                        self.managed_textures.remove(key);
                    }
                    TextureId::User(_) => todo!(),
                }
            }
            // upload textures
            self.set_textures(dev, queue, textures_delta.set);
        }

        // update screen size uniform buffer
        queue.write_buffer(
            &self.screen_size_buffer,
            0,
            cast_slice(&screen_size_logical),
        );

        {
            // total vertices and indices lengths
            let (vb_len, ib_len) = meshes.iter().fold((0, 0), |(vb_len, ib_len), mesh| {
                if let egui::epaint::Primitive::Mesh(ref m) = mesh.primitive {
                    (vb_len + m.vertices.len(), ib_len + m.indices.len())
                } else {
                    (vb_len, ib_len)
                }
            });
            if vb_len == 0 {
                return;
            }
            // resize if vertex or index buffer capcities are not enough
            if self.vb_len < vb_len {
                self.vb = dev.create_buffer(&BufferDescriptor {
                    label: Some("egui vertex buffer"),
                    size: vb_len as u64 * 20,
                    usage: BufferUsages::COPY_DST | BufferUsages::VERTEX,
                    mapped_at_creation: false,
                });
                self.vb_len = vb_len;
            }
            if self.ib_len < ib_len {
                self.ib = dev.create_buffer(&BufferDescriptor {
                    label: Some("egui index buffer"),
                    size: ib_len as u64 * 4,
                    usage: BufferUsages::COPY_DST | BufferUsages::INDEX,
                    mapped_at_creation: false,
                });
                self.ib_len = ib_len;
            }
            // create mutable slices for vertex and index buffers
            let mut vertex_buffer_mut = queue.write_buffer_with(
                &self.vb,
                0,
                NonZeroU64::new(
                    (self.vb_len * 20)
                        .try_into()
                        .expect("unreachable as usize is u64"),
                )
                .expect("vertex buffer length should not be zero"),
            );
            let mut index_buffer_mut = queue.write_buffer_with(
                &self.ib,
                0,
                NonZeroU64::new(
                    (self.ib_len * 4)
                        .try_into()
                        .expect("unreachable as usize is u64"),
                )
                .expect("index buffer length should not be zero"),
            );
            // offsets from where to start writing vertex or index buffer data
            let mut vb_offset = 0;
            let mut ib_offset = 0;
            for clipped_primitive in meshes {
                let ClippedPrimitive {
                    clip_rect,
                    primitive,
                } = clipped_primitive;

                // copy paste from official egui impl because i have no idea what this is :D
                let clip_min_x = scale * clip_rect.min.x;
                let clip_min_y = scale * clip_rect.min.y;
                let clip_max_x = scale * clip_rect.max.x;
                let clip_max_y = scale * clip_rect.max.y;
                let clip_min_x = clip_min_x.clamp(0.0, screen_size_physical[0] as f32);
                let clip_min_y = clip_min_y.clamp(0.0, screen_size_physical[1] as f32);
                let clip_max_x = clip_max_x.clamp(clip_min_x, screen_size_physical[0] as f32);
                let clip_max_y = clip_max_y.clamp(clip_min_y, screen_size_physical[1] as f32);

                let clip_min_x = clip_min_x.round() as u32;
                let clip_min_y = clip_min_y.round() as u32;
                let clip_max_x = clip_max_x.round() as u32;
                let clip_max_y = clip_max_y.round() as u32;

                let width = (clip_max_x - clip_min_x).max(1);
                let height = (clip_max_y - clip_min_y).max(1);

                // Clip scissor rectangle to target size.
                let clip_x = clip_min_x.min(screen_size_physical[0]);
                let clip_y = clip_min_y.min(screen_size_physical[1]);
                let clip_width = width.min(screen_size_physical[0] - clip_x);
                let clip_height = height.min(screen_size_physical[1] - clip_y);

                // Skip rendering with zero-sized clip areas.
                if clip_width == 0 || clip_height == 0 {
                    continue;
                }
                let scissor_rect = [clip_x, clip_y, clip_width, clip_height];
                match primitive {
                    egui::epaint::Primitive::Mesh(mesh) => {
                        let Mesh {
                            indices,
                            vertices,
                            texture_id,
                        } = mesh;

                        // offset upto where we want to write the vertices or indices.
                        let new_vb_offset = vb_offset + vertices.len() * 20; // multiply by vertex size as slice is &[u8]
                        let new_ib_offset = ib_offset + indices.len() * 4; // multiply by index size as slice is &[u8]
                                                                           // write from start offset to end offset
                        vertex_buffer_mut[vb_offset..new_vb_offset]
                            .copy_from_slice(cast_slice(&vertices));
                        index_buffer_mut[ib_offset..new_ib_offset]
                            .copy_from_slice(cast_slice(&indices));
                        // record draw call
                        self.draw_calls.push(
                            EguiDrawCalls::Mesh {
                                clip_rect: scissor_rect,
                                texture_id,
                                // vertex buffer offset is in bytes. so, we divide by size to get the "nth" vertex to use as base
                                base_vertex: (vb_offset / 20)
                                    .try_into()
                                    .expect("failed to fit vertex buffer offset into i32"),
                                // ib offset is in bytes. divided by index size, we get the starting and ending index to use for this draw call
                                index_start: (ib_offset / 4) as u32,
                                index_end: (new_ib_offset / 4) as u32,
                            }
                        );
                        // set end offsets as start offsets for next iteration
                        vb_offset = new_vb_offset;
                        ib_offset = new_ib_offset;
                    }
                    egui::epaint::Primitive::Callback(cb) => {
                        (cb.callback
                            .downcast_ref::<CallbackFn>()
                            .expect("failed to downcast egui callback fn")
                            .prepare)(dev, queue, &mut self.custom_data);
                        self.draw_calls.push(
                            EguiDrawCalls::Callback {
                                clip_rect: scissor_rect,
                                paint_callback: cb,
                                paint_callback_info: PaintCallbackInfo {
                                    viewport: Rect::from_min_size(
                                        Default::default(),
                                        screen_size_logical.into(),
                                    ),
                                    clip_rect,
                                    pixels_per_point: scale,
                                    screen_size_px: screen_size_physical,
                                },
                            }
                        );
                    }
                }
            }
        }
    }

    pub fn register_native_texture(
        &mut self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        texture_filter: wgpu::FilterMode,
    ) -> egui::TextureId {
        self.register_native_texture_with_sampler_options(
            device,
            view,
            wgpu::SamplerDescriptor {
                label: Some(format!("egui_user_image_{}", self.next_user_texture_id).as_str()),
                mag_filter: texture_filter,
                min_filter: texture_filter,
                ..Default::default()
            },
        )
    }

    #[allow(clippy::needless_pass_by_value)] // false positive
    pub fn register_native_texture_with_sampler_options(
        &mut self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        sampler_descriptor: wgpu::SamplerDescriptor<'_>,
    ) -> egui::TextureId {

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            compare: None,
            ..sampler_descriptor
        });

        let bindgroup = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(format!("egui_user_image_{}", self.next_user_texture_id).as_str()),
            layout: &self.texture_bindgroup_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),

                },
            ],
        });

        let key = egui::TextureId::User(self.next_user_texture_id);
        self.user_textures.insert(
            self.next_user_texture_id,
            EguiTexture {
                texture: None,
                view: None,
                bindgroup,
            },
        );

        self.next_user_texture_id += 1;

        key
    }
}

pub const SCREEN_SIZE_UNIFORM_BUFFER_BINDGROUP_ENTRY: [BindGroupLayoutEntry; 1] =
    [BindGroupLayoutEntry {
        binding: 0,
        visibility: ShaderStages::VERTEX,
        ty: BindingType::Buffer {
            ty: BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(16),
        },
        count: None,
    }];

pub const TEXTURE_BINDGROUP_ENTRIES: [BindGroupLayoutEntry; 2] = [
    BindGroupLayoutEntry {
        binding: 0,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Sampler(SamplerBindingType::Filtering),
        count: None,
    },
    BindGroupLayoutEntry {
        binding: 1,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Texture {
            sample_type: TextureSampleType::Float { filterable: true },
            view_dimension: TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    },
];
pub const VERTEX_BUFFER_LAYOUT: [VertexBufferLayout; 1] = [VertexBufferLayout {
    // vertex size
    array_stride: 20,
    step_mode: VertexStepMode::Vertex,
    attributes: &[
        // position x, y
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: 0,
            shader_location: 0,
        },
        // texture coordinates x, y
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: 8,
            shader_location: 1,
        },
        // color as rgba (unsigned bytes which will be turned into floats inside shader)
        VertexAttribute {
            format: VertexFormat::Unorm8x4,
            offset: 16,
            shader_location: 2,
        },
    ],
}];

pub const EGUI_PIPELINE_PRIMITIVE_STATE: PrimitiveState = PrimitiveState {
    topology: PrimitiveTopology::TriangleList,
    strip_index_format: None,
    front_face: FrontFace::Ccw,
    cull_mode: None,
    unclipped_depth: false,
    polygon_mode: PolygonMode::Fill,
    conservative: false,
};

pub const EGUI_PIPELINE_BLEND_STATE: BlendState = BlendState {
    color: BlendComponent {
        src_factor: BlendFactor::One,
        dst_factor: BlendFactor::OneMinusSrcAlpha,
        operation: BlendOperation::Add,
    },
    alpha: BlendComponent {
        src_factor: BlendFactor::OneMinusDstAlpha,
        dst_factor: BlendFactor::One,
        operation: BlendOperation::Add,
    },
};

// `Default::default` is not const. so, we have to manually fill the default values

pub const EGUI_LINEAR_SAMPLER_DESCRIPTOR: SamplerDescriptor = SamplerDescriptor {
    label: Some("linear sampler"),
    mag_filter: FilterMode::Linear,
    min_filter: FilterMode::Linear,
    mipmap_filter: FilterMode::Linear,
    address_mode_u: AddressMode::ClampToEdge,
    address_mode_v: AddressMode::ClampToEdge,
    address_mode_w: AddressMode::ClampToEdge,
    lod_min_clamp: 0.0,
    lod_max_clamp: f32::MAX,
    compare: None,
    anisotropy_clamp: None,
    border_color: None,
};

pub const EGUI_NEAREST_SAMPLER_DESCRIPTOR: SamplerDescriptor = SamplerDescriptor {
    label: Some("nearest sampler"),
    mag_filter: FilterMode::Nearest,
    min_filter: FilterMode::Nearest,
    mipmap_filter: FilterMode::Nearest,
    address_mode_u: AddressMode::ClampToEdge,
    address_mode_v: AddressMode::ClampToEdge,
    address_mode_w: AddressMode::ClampToEdge,
    lod_min_clamp: 0.0,
    lod_max_clamp: f32::MAX,
    compare: None,
    anisotropy_clamp: None,
    border_color: None,
};
