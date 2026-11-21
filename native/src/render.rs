//! wgpu renderer: I420 video (YUV->RGB in a shader, letterboxed) plus the egui UI on top.
use crate::codec::Frame;
use egui_wgpu::wgpu;
use std::sync::Arc;
use winit::window::Window;

const SHADER: &str = r#"
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tu: texture_2d<f32>;
@group(0) @binding(2) var tv: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;
// Part of the picture to show: xy = top-left, zw = size (0..1). Whole picture = (0, 0, 1, 1).
@group(0) @binding(4) var<uniform> view: vec4<f32>;
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> VO {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var o: VO;
    o.pos = vec4(p[i], 0.0, 1.0);
    o.uv = vec2((p[i].x + 1.0) * 0.5, 1.0 - (p[i].y + 1.0) * 0.5);
    return o;
}
@fragment fn fs(v: VO) -> @location(0) vec4<f32> {
    let uv = view.xy + v.uv * view.zw;
    // BT.601 limited range (matches both encoders)
    let y = 1.164 * (textureSample(ty, smp, uv).r - 0.0627);
    let u = textureSample(tu, smp, uv).r - 0.5;
    let w = textureSample(tv, smp, uv).r - 0.5;
    return vec4(y + 1.596 * w, y - 0.392 * u - 0.813 * w, y + 2.017 * u, 1.0);
}
"#;

struct VideoTex {
    w: u32,
    h: u32,
    planes: [wgpu::Texture; 3],
    bind: wgpu::BindGroup,
}

pub struct Gfx {
    pub window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    video: Option<VideoTex>,
    pub egui: egui_wgpu::Renderer,
    view_buf: wgpu::Buffer,
    /// Zoom (1 = whole picture) and the point (0..1) to keep centered when zoomed in.
    pub zoom: f32,
    pub focus: (f32, f32),
}

/// Backends to try, in order. Windows: DirectX 12 first (most dependable), then Vulkan.
/// If the last start hung or crashed while setting up graphics (a bad driver), a marker file is
/// left behind and this start begins with the next backend. PCLONE_GPU=dx12|vulkan forces one.
#[cfg(windows)]
const BACKENDS: [(&str, wgpu::Backends); 2] = [("DirectX 12", wgpu::Backends::DX12), ("Vulkan", wgpu::Backends::VULKAN)];
#[cfg(not(windows))]
const BACKENDS: [(&str, wgpu::Backends); 1] = [("default", wgpu::Backends::all())];

fn open_gpu(window: &Arc<Window>) -> anyhow::Result<(wgpu::Surface<'static>, wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let marker = crate::settings::data_dir().map(|d| d.join("gpu_attempt.txt"));
    let crashed: Option<usize> = marker.as_ref().and_then(|m| std::fs::read_to_string(m).ok()).and_then(|t| t.trim().parse().ok());
    let forced = std::env::var("PCLONE_GPU").ok().and_then(|v| BACKENDS.iter().position(|(n, _)| n.to_lowercase().replace(' ', "").contains(&v.to_lowercase())));
    let first = forced.unwrap_or(match crashed {
        Some(i) => {
            crate::plog!("graphics: last start got stuck on {}; trying the next one", BACKENDS[i % BACKENDS.len()].0);
            (i + 1) % BACKENDS.len()
        }
        None => 0,
    });
    let mut last_err = anyhow::anyhow!("no graphics backend");
    for k in 0..BACKENDS.len() {
        let i = (first + k) % BACKENDS.len();
        let (name, backends) = BACKENDS[i];
        if let Some(m) = &marker {
            let _ = std::fs::create_dir_all(m.parent().unwrap());
            let _ = std::fs::write(m, i.to_string());
        }
        crate::plog!("graphics: trying {name}");
        let attempt = || -> anyhow::Result<_> {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);
            let surface = instance.create_surface(window.clone())?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            }))?;
            let info = adapter.get_info();
            crate::plog!("graphics: {} ({:?}, driver {} {})", info.name, info.backend, info.driver, info.driver_info);
            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                // Ask only for what this GPU has (weak/software GPUs fail the default limits).
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                ..Default::default()
            }))?;
            Ok((surface, adapter, device, queue))
        };
        match attempt() {
            Ok(r) => {
                if let Some(m) = &marker {
                    let _ = std::fs::remove_file(m); // got through: no hang
                }
                crate::plog!("graphics: ready");
                return Ok(r);
            }
            Err(e) => {
                crate::plog!("graphics: {name} failed: {e:#}");
                last_err = e;
            }
        }
    }
    if let Some(m) = &marker {
        let _ = std::fs::remove_file(m);
    }
    Err(last_err)
}

impl Gfx {
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let (surface, adapter, device, queue) = open_gpu(&window)?;
        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB target: the shader already outputs display-ready values.
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let mut config = surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).ok_or_else(|| anyhow::anyhow!("surface unsupported"))?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::AutoNoVsync; // lowest latency
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let tex_entry = |b| wgpu::BindGroupLayoutEntry {
            binding: b,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: None, source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&layout)], ..Default::default() });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&pl),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let egui = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let view_buf = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Ok(Self { window, surface, device, queue, config, pipeline, layout, sampler, video: None, egui, view_buf, zoom: 1.0, focus: (0.5, 0.5) })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn upload(&mut self, f: &Frame) {
        let (w, h) = (f.w as u32, f.h as u32);
        if self.video.as_ref().is_none_or(|v| (v.w, v.h) != (w, h)) {
            let mk = |pw, ph| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let planes = [mk(w, h), mk(w / 2, h / 2), mk(w / 2, h / 2)];
            let views: Vec<_> = planes.iter().map(|t| t.create_view(&Default::default())).collect();
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&views[0]) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[1]) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[2]) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 4, resource: self.view_buf.as_entire_binding() },
                ],
            });
            self.video = Some(VideoTex { w, h, planes, bind });
        }
        let v = self.video.as_ref().unwrap();
        for (tex, data, pw, ph) in [(&v.planes[0], &f.y, w, h), (&v.planes[1], &f.u, w / 2, h / 2), (&v.planes[2], &f.v, w / 2, h / 2)] {
            self.queue.write_texture(
                tex.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(pw), rows_per_image: Some(ph) },
                wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
            );
        }
    }

    pub fn clear_video(&mut self) {
        self.video = None;
    }

    /// Letterboxed video rect in physical pixels: (x, y, w, h).
    pub fn video_rect(&self) -> Option<(f32, f32, f32, f32)> {
        let v = self.video.as_ref()?;
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let scale = (sw / v.w as f32).min(sh / v.h as f32);
        let (w, h) = (v.w as f32 * scale, v.h as f32 * scale);
        Some(((sw - w) / 2.0, (sh - h) / 2.0, w, h))
    }

    /// The part of the picture on screen: (left, top, size), all 0..1 of the picture.
    pub fn view_window(&self) -> (f32, f32, f32) {
        let s = 1.0 / self.zoom.max(1.0);
        let x = (self.focus.0 - s / 2.0).clamp(0.0, 1.0 - s);
        let y = (self.focus.1 - s / 2.0).clamp(0.0, 1.0 - s);
        (x, y, s)
    }

    pub fn render(&mut self, show_video: bool, out: egui::FullOutput, ctx: &egui::Context) {
        let (vx, vy, vs) = self.view_window();
        let bytes: Vec<u8> = [vx, vy, vs, vs].iter().flat_map(|f| f.to_le_bytes()).collect();
        self.queue.write_buffer(&self.view_buf, 0, &bytes);
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let ppp = out.pixels_per_point;
        let jobs = ctx.tessellate(out.shapes, ppp);
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [self.config.width, self.config.height], pixels_per_point: ppp };
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                self.egui.update_texture(&self.device, &self.queue, *id, d);
            }
        }
        let mut enc = self.device.create_command_encoder(&Default::default());
        let extra = self.egui.update_buffers(&self.device, &self.queue, &mut enc, &jobs, &screen);
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(if show_video { wgpu::Color::BLACK } else { wgpu::Color { r: 0.07, g: 0.07, b: 0.08, a: 1.0 } }), store: wgpu::StoreOp::Store },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            if show_video {
                if let (Some(v), Some((x, y, w, h))) = (&self.video, self.video_rect()) {
                    pass.set_viewport(x, y, w, h, 0.0, 1.0);
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &v.bind, &[]);
                    pass.draw(0..3, 0..1);
                    pass.set_viewport(0.0, 0.0, self.config.width as f32, self.config.height as f32, 0.0, 1.0);
                }
            }
            self.egui.render(&mut pass, &jobs, &screen);
        }
        self.queue.submit(extra.into_iter().chain([enc.finish()]));
        self.window.pre_present_notify();
        self.queue.present(frame);
        for id in &out.textures_delta.free {
            self.egui.free_texture(id);
        }
    }
}
