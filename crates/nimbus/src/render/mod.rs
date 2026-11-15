//! wgpu renderer: video plane (YUV → RGB + sharpening) and egui on top.

use std::time::Instant;

use anyhow::{Context, Result};
use nimbus_media::video::{Matrix, PixelLayout, VideoFrame};
use raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle, RawDisplayHandle};

use crate::settings::PresentPref;

/// A display handle that can be moved to the render thread.
#[derive(Debug, Clone, Copy)]
pub struct SendDisplayHandle(pub RawDisplayHandle);
unsafe impl Send for SendDisplayHandle {}
unsafe impl Sync for SendDisplayHandle {}

impl HasDisplayHandle for SendDisplayHandle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(self.0) })
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VideoParams {
    texel: [f32; 2],
    sharpness: f32,
    layout: u32,
    full_range: u32,
    matrix: u32,
    _pad: [u32; 2],
}

struct VideoTextures {
    width: u32,
    height: u32,
    layout: PixelLayout,
    planes: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    present_modes: Vec<wgpu::PresentMode>,
    pub adapter_name: String,

    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    dummy: wgpu::Texture,
    video: Option<VideoTextures>,
    video_params: VideoParams,

    egui: egui_wgpu::Renderer,
    screenshot: Option<std::path::PathBuf>,
}

pub struct FrameInfo {
    pub presented_at: Instant,
}

/// Frames allowed in flight. One keeps latency minimal; "V-Sync" allows a
/// second so a frame arriving just before vblank never waits behind the one
/// on screen (smoother, at most one refresh more latency).
fn frame_latency(pref: PresentPref) -> u32 {
    match pref {
        PresentPref::VSync => 2,
        _ => 1,
    }
}

fn pick_present_mode(pref: PresentPref, available: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    use wgpu::PresentMode as P;
    let wanted: &[P] = match pref {
        PresentPref::LowLatency => &[P::Mailbox, P::Fifo],
        PresentPref::VSync => &[P::Fifo],
        PresentPref::Immediate => &[P::Immediate, P::Mailbox, P::Fifo],
    };
    wanted
        .iter()
        .copied()
        .find(|m| available.contains(m))
        .unwrap_or(P::Fifo)
}

impl Renderer {
    pub fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        present: PresentPref,
    ) -> Result<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .context("no compatible GPU adapter")?;
        let info = adapter.get_info();
        tracing::info!(name = %info.name, backend = ?info.backend, "GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("nimbus"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("request_device")?;

        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB target: our shader outputs gamma-encoded (video) values.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_modes = caps.present_modes.clone();
        let present_mode = pick_present_mode(present, &present_modes);
        tracing::info!(?format, ?present_mode, available = ?present_modes, "surface");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | (caps.usages & wgpu::TextureUsages::COPY_SRC),
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: frame_latency(present),
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video.wgsl").into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(2),
                tex_entry(3),
                tex_entry(4),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video params"),
            size: std::mem::size_of::<VideoParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dummy = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let egui = egui_wgpu::Renderer::new(
            &device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );

        Ok(Self {
            device,
            queue,
            surface,
            config,
            present_modes,
            adapter_name: format!("{} ({:?})", info.name, info.backend),
            pipeline,
            bind_layout,
            sampler,
            params,
            dummy,
            video: None,
            video_params: VideoParams {
                texel: [0.0; 2],
                sharpness: 0.0,
                layout: 0,
                full_range: 0,
                matrix: 1,
                _pad: [0; 2],
            },
            egui,
            screenshot: None,
        })
    }

    /// Save the next rendered frame as a PNG (debugging / bug reports).
    pub fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.screenshot = Some(path);
    }

    fn capture(&self, encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture) -> (wgpu::Buffer, u32) {
        let (w, h) = (self.config.width, self.config.height);
        let bpr = (w * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot"),
            size: (bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        (buffer, bpr)
    }

    fn save_capture(&self, buffer: wgpu::Buffer, bpr: u32, path: &std::path::Path) {
        let (w, h) = (self.config.width, self.config.height);
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        if !matches!(rx.recv(), Ok(Ok(()))) {
            tracing::warn!("screenshot map failed");
            return;
        }
        let Ok(data) = slice.get_mapped_range() else {
            tracing::warn!("screenshot: mapped range unavailable");
            return;
        };
        let bgra = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let line = &data[(row * bpr) as usize..(row * bpr + w * 4) as usize];
            for px in line.chunks_exact(4) {
                if bgra {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        drop(data);
        buffer.unmap();
        match image::save_buffer(path, &rgba, w, h, image::ExtendedColorType::Rgba8) {
            Ok(()) => tracing::info!(path = %path.display(), "screenshot saved"),
            Err(e) => tracing::warn!("screenshot failed: {e}"),
        }
    }

    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.config.width == width && self.config.height == height {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_present(&mut self, pref: PresentPref) {
        let mode = pick_present_mode(pref, &self.present_modes);
        let latency = frame_latency(pref);
        if mode != self.config.present_mode || latency != self.config.desired_maximum_frame_latency {
            self.config.present_mode = mode;
            self.config.desired_maximum_frame_latency = latency;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn clear_video(&mut self) {
        self.video = None;
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    fn ensure_textures(&mut self, frame: &VideoFrame) {
        if let Some(v) = &self.video
            && v.width == frame.width
            && v.height == frame.height
            && v.layout == frame.layout
        {
            return;
        }
        let make = |w: u32, h: u32, format: wgpu::TextureFormat, label: &str| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let planes: Vec<wgpu::Texture> = match frame.layout {
            PixelLayout::Nv12 => vec![
                make(frame.planes[0].width, frame.planes[0].height, wgpu::TextureFormat::R8Unorm, "Y"),
                make(frame.planes[1].width, frame.planes[1].height, wgpu::TextureFormat::Rg8Unorm, "UV"),
            ],
            PixelLayout::Yuv420p => vec![
                make(frame.planes[0].width, frame.planes[0].height, wgpu::TextureFormat::R8Unorm, "Y"),
                make(frame.planes[1].width, frame.planes[1].height, wgpu::TextureFormat::R8Unorm, "U"),
                make(frame.planes[2].width, frame.planes[2].height, wgpu::TextureFormat::R8Unorm, "V"),
            ],
        };
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let y = view(&planes[0]);
        let u = view(&planes[1]);
        let v = planes.get(2).map(view).unwrap_or_else(|| view(&self.dummy));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&y),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&u),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&v),
                },
            ],
        });
        tracing::info!(
            width = frame.width,
            height = frame.height,
            layout = ?frame.layout,
            "video textures (re)created"
        );
        self.video = Some(VideoTextures {
            width: frame.width,
            height: frame.height,
            layout: frame.layout,
            planes,
            bind_group,
        });
    }

    /// Upload a decoded frame.
    pub fn upload(&mut self, frame: &VideoFrame, sharpness: f32) {
        self.ensure_textures(frame);
        let Some(v) = &self.video else { return };
        for (i, tex) in v.planes.iter().enumerate() {
            let p = frame.planes[i];
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                frame.plane(i),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(p.stride as u32),
                    rows_per_image: Some(p.height),
                },
                wgpu::Extent3d {
                    width: p.width,
                    height: p.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.video_params = VideoParams {
            texel: [1.0 / frame.width as f32, 1.0 / frame.height as f32],
            sharpness,
            layout: match frame.layout {
                PixelLayout::Nv12 => 0,
                PixelLayout::Yuv420p => 1,
            },
            full_range: frame.full_range as u32,
            matrix: match frame.matrix {
                Matrix::Bt601 => 0,
                Matrix::Bt709 => 1,
            },
            _pad: [0; 2],
        };
        self.queue
            .write_buffer(&self.params, 0, bytemuck::bytes_of(&self.video_params));
    }

    pub fn set_sharpness(&mut self, sharpness: f32) {
        if self.video_params.sharpness != sharpness {
            self.video_params.sharpness = sharpness;
            self.queue
                .write_buffer(&self.params, 0, bytemuck::bytes_of(&self.video_params));
        }
    }

    /// Render one frame. Returns `None` when the surface wasn't available.
    pub fn render(
        &mut self,
        draw_video: bool,
        egui_ctx: &egui::Context,
        mut output: egui::FullOutput,
        pixels_per_point: f32,
    ) -> Option<FrameInfo> {
        let mut textures = std::mem::take(&mut output.textures_delta);
        for (id, deltas) in textures.set.drain() {
            for delta in &deltas {
                self.egui.update_texture(&self.device, &self.queue, id, delta);
            }
        }
        let result = self.render_inner(draw_video, egui_ctx, output, pixels_per_point);
        for id in textures.free.drain() {
            self.egui.free_texture(&id);
        }
        result
    }

    fn render_inner(
        &mut self,
        draw_video: bool,
        egui_ctx: &egui::Context,
        output: egui::FullOutput,
        pixels_per_point: f32,
    ) -> Option<FrameInfo> {

        // When the window is hidden we normally skip rendering entirely; a
        // pending screenshot is rendered offscreen instead.
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                Some(t)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                tracing::debug!("surface outdated/lost, reconfiguring");
                self.surface.configure(&self.device, &self.config);
                None
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                tracing::trace!("surface occluded");
                None
            }
            other => {
                tracing::debug!(
                    timeout = matches!(other, wgpu::CurrentSurfaceTexture::Timeout),
                    "no surface texture"
                );
                None
            }
        };
        let offscreen = match (&frame, &self.screenshot) {
            (None, Some(_)) => Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("offscreen"),
                size: wgpu::Extent3d {
                    width: self.config.width,
                    height: self.config.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.config.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })),
            (None, None) => return None,
            _ => None,
        };
        let target: &wgpu::Texture = match (&frame, &offscreen) {
            (Some(f), _) => &f.texture,
            (None, Some(t)) => t,
            (None, None) => unreachable!(),
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let primitives = egui_ctx.tessellate(output.shapes, pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let extra = self
            .egui
            .update_buffers(&self.device, &self.queue, &mut encoder, &primitives, &screen);

        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("main"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();

            if draw_video && let Some(v) = &self.video {
                let (sw, sh) = (self.config.width as f32, self.config.height as f32);
                let scale = (sw / v.width as f32).min(sh / v.height as f32);
                let (w, h) = (v.width as f32 * scale, v.height as f32 * scale);
                let (x, y) = ((sw - w) / 2.0, (sh - h) / 2.0);
                pass.set_viewport(x.floor(), y.floor(), w.round(), h.round(), 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &v.bind_group, &[]);
                pass.draw(0..3, 0..1);
                pass.set_viewport(0.0, 0.0, sw, sh, 0.0, 1.0);
            }

            self.egui.render(&mut pass, &primitives, &screen);
        }

        let shot = self
            .screenshot
            .take()
            .map(|p| (self.capture(&mut encoder, target), p));
        self.queue
            .submit(extra.into_iter().chain([encoder.finish()]));
        let presented = frame.is_some();
        if let Some(frame) = frame {
            self.queue.present(frame);
        }
        let presented_at = Instant::now();
        if let Some(((buf, bpr), path)) = shot {
            self.save_capture(buf, bpr, &path);
        }
        if !presented {
            return None;
        }

        Some(FrameInfo { presented_at })
    }
}
