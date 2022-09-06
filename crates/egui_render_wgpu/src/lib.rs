use std::sync::Arc;

use egui_backend::{GfxBackend, WindowBackend};
use painter::EguiPainter;
use pollster::block_on;
use wgpu::{
    Adapter, Backends, CommandEncoder, CommandEncoderDescriptor, Device, DeviceDescriptor,
    Instance, LoadOp, Operations, PowerPreference, PresentMode, Queue, RenderPassColorAttachment,
    RenderPassDescriptor, RequestAdapterOptions, Surface, SurfaceConfiguration, SurfaceTexture,
    TextureAspect, TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
    TextureViewDimension,
};
pub mod painter;
pub use wgpu;
pub struct WgpuBackend {
    /// wgpu data
    pub instance: Instance,
    pub adapter: Arc<Adapter>,
    pub device: Arc<Device>,
    pub queue: Arc<Queue>,
    /// contains egui specific wgpu data like textures or buffers or pipelines etc..
    pub painter: EguiPainter,
    pub surface: Surface,
    pub surface_config: SurfaceConfiguration,
    pub surface_current_image: Option<SurfaceTexture>,
    pub surface_view: Option<TextureView>,
    pub command_encoders: Vec<CommandEncoder>,
}

#[derive(Debug, Default)]
pub struct WgpuSettings {}
impl GfxBackend for WgpuBackend {
    type GfxBackendSettings = WgpuSettings;

    fn new(
        window_info_for_gfx: egui_backend::WindowInfoForGfx,
        _settings: Self::GfxBackendSettings,
    ) -> Self {
        assert!(
            window_info_for_gfx.opengl_context.is_none(),
            "wgpu backend received opengl window context"
        );
        let instance = Instance::new(Backends::VULKAN);
        let surface = unsafe { instance.create_surface(&window_info_for_gfx) };

        let adapter = Arc::new(
            block_on(instance.request_adapter(&RequestAdapterOptions {
                power_preference: PowerPreference::default(),
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            }))
            .expect("failed to get adapter"),
        );

        let (device, queue) = block_on(adapter.request_device(
            &DeviceDescriptor {
                label: Some("my wgpu device"),
                features: Default::default(),
                limits: Default::default(),
            },
            Default::default(),
        ))
        .expect("failed to create wgpu device");

        let device = Arc::new(device);
        let queue = Arc::new(queue);
        let mut surface_format = None;
        // only use Srgb formats
        for format in surface.get_supported_formats(&adapter) {
            match format {
                TextureFormat::Rgba8UnormSrgb => surface_format = Some(format),
                TextureFormat::Bgra8UnormSrgb => surface_format = Some(format),
                _ => {}
            };
        }
        let surface_format = surface_format.expect("failed to get a suitable format");
        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: 0,
            height: 0,
            present_mode: PresentMode::Fifo,
        };
        let painter = EguiPainter::new(&device, surface_format);

        Self {
            instance,
            adapter,
            device,
            queue,
            painter,
            surface,
            surface_config,
            surface_view: None,
            surface_current_image: None,
            command_encoders: Vec::new(),
        }
    }

    fn prepare_frame<W: WindowBackend>(
        &mut self,
        framebuffer_size_update: Option<[u32; 2]>,
        window_backend: &mut W,
    ) {
        if let Some(size) = framebuffer_size_update {
            self.surface_config.width = size[0];
            self.surface_config.height = size[1];
            self.surface.configure(&self.device, &self.surface_config);
        }
        assert!(self.surface_current_image.is_none());
        assert!(self.surface_view.is_none());
        let current_surface_image = self.surface.get_current_texture().unwrap_or_else(|e| {
            let phy_fb_size = window_backend.get_live_physical_size_framebuffer();
            self.surface_config.width = phy_fb_size[0];
            self.surface_config.height = phy_fb_size[1];
            self.surface.configure(&self.device, &self.surface_config);
            self.surface.get_current_texture().expect(&format!(
                "failed to get surface even after reconfiguration. {e}"
            ))
        });
        let surface_view = current_surface_image
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
        self.surface_current_image = Some(current_surface_image);
    }

    fn prepare_render(&mut self, egui_gfx_output: egui_backend::EguiGfxOutput) {
        self.painter
            .upload_egui_data(&self.device, &self.queue, egui_gfx_output);
    }

    fn render(&mut self) {
        let mut command_encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("egui command encoder"),
            });
        {
            let mut egui_pass = command_encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("egui render pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: self
                        .surface_view
                        .as_ref()
                        .expect("failed ot get surface view for egui render pass creation"),
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: true,
                    },
                })],
                depth_stencil_attachment: None,
            });
            self.painter.draw_egui_with_renderpass(&mut egui_pass);
        }
        self.command_encoders.push(command_encoder);
    }

    fn present(&mut self) {
        self.queue.submit(
            std::mem::take(&mut self.command_encoders)
                .into_iter()
                .map(|encoder| encoder.finish()),
        );
        {
            self.surface_view
                .take()
                .expect("failed to get surface view to present");
        }
        self.surface_current_image
            .take()
            .expect("failed to surface texture to preset")
            .present();
    }
}
