//! GPU compositor (wgpu: Vulkan / Metal / DirectX 12 / OpenGL). Layers are prepared on the CPU
//! (crop, filters), uploaded as textures, drawn as alpha-blended quads, and read back as a Frame.

use rustin_core::{CompositeBackend, Frame, Layer};

const SHADER: &str = include_str!("shader.wgsl");
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct Target {
    w: u32,
    h: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded_row: u32,
}

struct LayerGpu {
    w: u32,
    h: u32,
    texture: wgpu::Texture,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

pub struct GpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    adapter_name: String,
    software: bool,
    target: Option<Target>,
    layers: Vec<LayerGpu>,
}

impl GpuCompositor {
    /// Opens a GPU (preferring a real one). Fails if the machine has no usable adapter. Never
    /// panics: driver trouble is reported as an error so the caller can fall back to the CPU.
    pub fn new() -> Result<Self, String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(Self::open))
            .map_err(|_| "the graphics driver crashed while starting".to_string())?
    }

    fn open() -> Result<Self, String> {
        // Vulkan / Metal / DirectX 12 only. The OpenGL backend is left out on purpose: it needs
        // its own EGL context, which clashes with the OpenGL context the app window already owns.
        let backends = wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends, ..Default::default() });
        let request = |force_fallback| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: force_fallback,
            }))
        };
        let adapter = request(false).or_else(|| request(true)).ok_or("no GPU adapter found")?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("rustin-gpu"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .map_err(|e| format!("couldn't open the GPU: {e}"))?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("layer"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[&bgl], push_constant_ranges: &[] });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("composite"),
            layout: Some(&layout),
            vertex: wgpu::VertexState { module: &module, entry_point: "vs_main", buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    // Straight-alpha "over" for colour; leave the canvas alpha alone.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            bgl,
            adapter_name: info.name.clone(),
            software: info.device_type == wgpu::DeviceType::Cpu,
            target: None,
            layers: Vec::new(),
        })
    }

    pub fn adapter_name(&self) -> &str { &self.adapter_name }

    /// True when the "GPU" is a software renderer (no real graphics hardware found).
    pub fn is_software(&self) -> bool { self.software }

    fn ensure_target(&mut self, w: u32, h: u32) {
        if self.target.as_ref().map_or(false, |t| t.w == w && t.h == h) {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("canvas"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let padded_row = (w * 4 + 255) / 256 * 256;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: padded_row as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.target = Some(Target { w, h, texture, view, readback, padded_row });
    }

    fn ensure_layer(&mut self, i: usize, w: u32, h: u32) {
        if self.layers.len() <= i {
            // placeholder slots are filled just below
        }
        if self.layers.get(i).map_or(false, |l| l.w == w && l.h == h) {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layer"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uniform = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("layer params"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layer"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
            ],
        });
        let slot = LayerGpu { w, h, texture, uniform, bind_group };
        if i < self.layers.len() {
            self.layers[i] = slot;
        } else {
            self.layers.push(slot);
        }
    }
}

fn params_bytes(rect: [f32; 4], info: [f32; 4], canvas: [f32; 4]) -> [u8; 48] {
    let mut out = [0u8; 48];
    for (i, v) in rect.iter().chain(info.iter()).chain(canvas.iter()).enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
    }
    out
}

impl CompositeBackend for GpuCompositor {
    fn name(&self) -> String {
        format!("GPU ({})", self.adapter_name)
    }

    fn composite(&mut self, width: u32, height: u32, background: [u8; 4], layers: &[Layer]) -> Result<Frame, String> {
        if width == 0 || height == 0 {
            return Err("empty canvas".into());
        }
        let max = self.device.limits().max_texture_dimension_2d;
        if width > max || height > max || layers.iter().any(|l| l.frame.width > max || l.frame.height > max) {
            return Err(format!("texture larger than this GPU supports ({max} px)"));
        }
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        self.ensure_target(width, height);
        for (i, l) in layers.iter().enumerate() {
            self.ensure_layer(i, l.frame.width, l.frame.height);
            let g = &self.layers[i];
            self.queue.write_texture(
                wgpu::ImageCopyTexture { texture: &g.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &l.frame.data,
                wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(l.frame.width * 4), rows_per_image: Some(l.frame.height) },
                wgpu::Extent3d { width: l.frame.width, height: l.frame.height, depth_or_array_layers: 1 },
            );
            let out_w = (l.frame.width as f32 * l.scale).round();
            let out_h = (l.frame.height as f32 * l.scale).round();
            self.queue.write_buffer(
                &g.uniform,
                0,
                &params_bytes(
                    [l.x as f32, l.y as f32, out_w, out_h],
                    [l.scale, l.op as f32 / 255.0, l.frame.width as f32, l.frame.height as f32],
                    [width as f32, height as f32, 0.0, 0.0],
                ),
            );
        }

        let target = self.target.as_ref().unwrap();
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let clear = wgpu::Color {
                r: background[0] as f64 / 255.0,
                g: background[1] as f64 / 255.0,
                b: background[2] as f64 / 255.0,
                a: background[3] as f64 / 255.0,
            };
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            for g in self.layers.iter().take(layers.len()) {
                pass.set_bind_group(0, &g.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::ImageCopyBuffer {
                buffer: &target.readback,
                layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(target.padded_row), rows_per_image: Some(height) },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(enc.finish()));

        let slice = target.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("readback failed: {e}"))?;
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        {
            let mapped = slice.get_mapped_range();
            for row in 0..height as usize {
                let start = row * target.padded_row as usize;
                data.extend_from_slice(&mapped[start..start + width as usize * 4]);
            }
        }
        target.readback.unmap();
        if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
            return Err(err.to_string());
        }
        Ok(Frame { width, height, pts_ns: 0, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustin_core::{
        source::{ColorSource, TestPatternSource},
        Compositor, Scene, SceneItem,
    };

    fn busy_scene() -> Scene {
        let mut s = Scene::new("busy");
        s.add(SceneItem::new(Box::new(TestPatternSource::new("bg", 97, 61))));
        s.add(SceneItem::new(Box::new(TestPatternSource::new("scaled", 40, 30))).at(-7, 11).scaled(1.7));
        s.add(SceneItem::new(Box::new(ColorSource::new("fade", 30, 30, [10, 200, 90, 255]))).at(60, 40).with_opacity(0.4));
        s.add(SceneItem::new(Box::new(TestPatternSource::new("off", 50, 50))).at(80, -20).scaled(0.6));
        let mut g = SceneItem::new(Box::new(TestPatternSource::new("gray", 33, 33))).at(5, 5);
        g.grayscale = true;
        s.add(g);
        s
    }

    fn gpu() -> Option<GpuCompositor> {
        match GpuCompositor::new() {
            Ok(g) => {
                eprintln!("testing on: {}", g.adapter_name());
                Some(g)
            }
            Err(e) => {
                eprintln!("no GPU available ({e}); skipped");
                None
            }
        }
    }

    /// (max channel difference, fraction of pixels within `tol`)
    fn compare(a: &Frame, b: &Frame, tol: u8) -> (u8, f64) {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let mut max = 0u8;
        let mut close = 0usize;
        for (p, q) in a.data.chunks_exact(4).zip(b.data.chunks_exact(4)) {
            let d = (0..4).map(|c| p[c].abs_diff(q[c])).max().unwrap();
            max = max.max(d);
            if d <= tol { close += 1; }
        }
        (max, close as f64 / (a.data.len() / 4) as f64)
    }

    #[test]
    fn gpu_matches_cpu_on_a_busy_scene() {
        let Some(g) = gpu() else { return };
        let mut cpu = Compositor::new(97, 61);
        cpu.threads = 1;
        let reference = cpu.render(&mut busy_scene(), 123_000_000);

        let mut comp = Compositor::new(97, 61);
        comp.backend = Some(Box::new(g));
        let out = comp.render(&mut busy_scene(), 123_000_000);
        assert!(comp.backend.is_some(), "GPU backend failed: {:?}", comp.backend_error);
        let (max, frac) = compare(&reference, &out, 3);
        let (_, exact) = compare(&reference, &out, 0);
        eprintln!("GPU vs CPU: {:.2}% of pixels within 3 levels, {:.2}% bit-identical, max channel diff {max}", frac * 100.0, exact * 100.0);
        assert!(frac > 0.995, "only {:.2}% of pixels match (max diff {max})", frac * 100.0);
        assert_eq!(out.pts_ns, 123_000_000);
    }

    #[test]
    fn gpu_handles_resizes_empty_scenes_and_many_layers() {
        let Some(g) = gpu() else { return };
        let mut comp = Compositor::new(64, 36);
        comp.backend = Some(Box::new(g));
        // Empty scene = just the background.
        let mut empty = Scene::new("e");
        let f = comp.render(&mut empty, 0);
        assert_eq!(f.pixel(10, 10), [0, 0, 0, 255]);
        // Many layers, then a different canvas size and layer sizes.
        let mut many = Scene::new("m");
        for i in 0..12 {
            many.add(SceneItem::new(Box::new(ColorSource::new("c", 8, 8, [i * 20, 0, 255 - i * 20, 255]))).at(i as i32 * 4, i as i32 * 2));
        }
        let f = comp.render(&mut many, 0);
        assert_eq!(f.pixel(4 * 11 + 2, 2 * 11 + 2), [220, 0, 35, 255]); // last layer on top
        comp.width = 128;
        comp.height = 72;
        let f = comp.render(&mut many, 0);
        assert_eq!((f.width, f.height), (128, 72));
        assert!(comp.backend.is_some(), "{:?}", comp.backend_error);
    }

    #[test]
    fn gpu_transparent_pixels_do_not_darken_the_background() {
        let Some(g) = gpu() else { return };
        let mut comp = Compositor::new(8, 8);
        comp.background = [200, 100, 50, 255];
        comp.backend = Some(Box::new(g));
        let mut scene = Scene::new("t");
        // Fully transparent red must leave the background untouched.
        scene.add(SceneItem::new(Box::new(ColorSource::new("clear", 8, 8, [255, 0, 0, 0]))));
        let f = comp.render(&mut scene, 0);
        assert_eq!(f.pixel(3, 3), [200, 100, 50, 255]);
    }
}
