//! CPU vs GPU compositing at 1080p: cargo run --release -p rustin-gpu --example bench_gpu
use rustin_core::{Compositor, Frame, Scene, SceneItem, Source, SourceSpec};
use rustin_gpu::GpuCompositor;
use std::time::Instant;

struct Static { name: String, frame: Frame }
impl Static {
    fn new(name: &str, w: u32, h: u32) -> Self {
        let mut f = Frame::new(w, h, [0, 0, 0, 255]);
        for (i, px) in f.data.chunks_exact_mut(4).enumerate() {
            let v = (i.wrapping_mul(2654435761) >> 24) as u8;
            px[0] = v; px[1] = v.wrapping_add(60); px[2] = v.wrapping_add(120);
        }
        Self { name: name.into(), frame: f }
    }
}
impl Source for Static {
    fn name(&self) -> &str { &self.name }
    fn set_name(&mut self, n: &str) { self.name = n.into(); }
    fn kind(&self) -> &'static str { "Static" }
    fn spec(&self) -> SourceSpec { SourceSpec::TestPattern { name: self.name.clone(), width: self.frame.width, height: self.frame.height } }
    fn size(&self) -> (u32, u32) { (self.frame.width, self.frame.height) }
    fn render(&mut self, _t: u64) -> Frame { self.frame.clone() }
}

fn scene() -> Scene {
    let mut s = Scene::new("bench");
    s.add(SceneItem::new(Box::new(Static::new("display", 1920, 1080))));
    s.add(SceneItem::new(Box::new(Static::new("window", 1280, 720))).at(100, 100));
    s.add(SceneItem::new(Box::new(Static::new("cam", 1280, 720))).at(1300, 700).scaled(0.5).with_opacity(0.9));
    s.add(SceneItem::new(Box::new(Static::new("small", 960, 540))).at(50, 500));
    s
}

fn time(comp: &mut Compositor, label: &str) {
    let mut sc = scene();
    for _ in 0..5 { comp.render(&mut sc, 0); }
    let n = 60;
    let t0 = Instant::now();
    for i in 0..n { std::hint::black_box(comp.render(&mut sc, i)); }
    let ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("{label:<34} {ms:6.1} ms/frame  ({:.0} fps max)", 1000.0 / ms);
}

fn main() {
    println!("1920x1080, 4 sources, {} CPU core(s)", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let mut cpu = Compositor::new(1920, 1080);
    time(&mut cpu, "CPU");
    match GpuCompositor::new() {
        Ok(g) => {
            let label = format!("GPU: {}{}", g.adapter_name(), if g.is_software() { " [software]" } else { "" });
            let mut gpu = Compositor::new(1920, 1080);
            gpu.backend = Some(Box::new(g));
            time(&mut gpu, &label);
            if gpu.backend.is_none() { println!("(GPU backend failed: {:?})", gpu.backend_error); }
        }
        Err(e) => println!("GPU unavailable: {e}"),
    }
}
