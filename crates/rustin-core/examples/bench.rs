//! Compositor benchmark: cargo run --release -p rustin-core --example bench
use rustin_core::{Compositor, Frame, Scene, SceneItem, Source, SourceSpec};
use std::time::Instant;

/// Pre-generated opaque frame, like a screen capture.
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

fn main() {
    let (w, h) = (1920, 1080);
    let mut scene = Scene::new("bench");
    scene.add(SceneItem::new(Box::new(Static::new("display", 1920, 1080))));
    scene.add(SceneItem::new(Box::new(Static::new("window", 1280, 720))).at(100, 100));
    scene.add(SceneItem::new(Box::new(Static::new("cam", 1280, 720))).at(1300, 700).scaled(0.5).with_opacity(0.9));
    let mut gray = SceneItem::new(Box::new(Static::new("gray", 960, 540))).at(50, 500);
    gray.grayscale = true;
    scene.add(gray);
    let mut comp = Compositor::new(w, h);
    for _ in 0..5 { comp.render(&mut scene, 0); }
    let n = 100;
    let t0 = Instant::now();
    for i in 0..n { std::hint::black_box(comp.render(&mut scene, i)); }
    let ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("{w}x{h}, 4 sources: {ms:.1} ms/frame ({:.0} fps max)  [60 fps needs < 16.7 ms]", 1000.0 / ms);
    // Breakdown of fixed costs.
    let mut srcs = [Static::new("a", 1920, 1080), Static::new("b", 1280, 720), Static::new("c", 1280, 720), Static::new("d", 960, 540)];
    let t = Instant::now();
    for i in 0..n { for s in srcs.iter_mut() { std::hint::black_box(s.render(i)); } }
    println!("  source frame clones : {:.1} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    let t = Instant::now();
    for _ in 0..n { std::hint::black_box(Frame::new(w, h, [0, 0, 0, 255])); }
    println!("  canvas background   : {:.1} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    println!("cores: {}", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
}
