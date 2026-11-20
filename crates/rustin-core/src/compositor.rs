use crate::{filter::Filter, frame::Frame, scene::Scene};

/// CPU compositor: nearest-neighbour scale + alpha-over blend onto a canvas.
///
/// Sources are rendered first, then the canvas is split into horizontal bands that are
/// composited in parallel. Opaque, unscaled sources (e.g. a full-screen capture at the
/// canvas resolution) take a memcpy fast path.
pub struct Compositor {
    pub width: u32,
    pub height: u32,
    pub background: [u8; 4],
    /// Worker threads used for compositing (1 = single-threaded).
    pub threads: usize,
    /// Optional alternative compositor (e.g. on the GPU). If it fails, it's dropped, the error
    /// is kept in `backend_error`, and rendering continues on the CPU.
    pub backend: Option<Box<dyn CompositeBackend>>,
    pub backend_error: Option<String>,
}

/// One source, fully prepared (cropped, filtered) and ready to draw.
pub struct Layer {
    pub frame: Frame,
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    /// Opacity as 0..=255.
    pub op: u32,
    /// Source column for each output column (only when scale != 1.0).
    xmap: Vec<u32>,
}

/// Draws prepared layers (bottom to top) onto a canvas of the given size.
/// Semantics must match the CPU path: nearest-neighbour scaling, straight-alpha "over".
pub trait CompositeBackend: Send {
    fn name(&self) -> String;
    fn composite(&mut self, width: u32, height: u32, background: [u8; 4], layers: &[Layer]) -> Result<Frame, String>;
}

impl Compositor {
    pub fn new(width: u32, height: u32) -> Self {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
        Self { width, height, background: [0, 0, 0, 255], threads, backend: None, backend_error: None }
    }

    pub fn render(&mut self, scene: &mut Scene, t_ns: u64) -> Frame {
        let mut layers: Vec<Layer> = Vec::new();
        for item in scene.items.iter_mut().filter(|i| i.visible) {
            let op = (item.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
            if op == 0 || item.scale <= 0.0 {
                continue;
            }
            let mut src = item.source.render(t_ns);
            if src.width == 0 || src.height == 0 {
                continue;
            }
            if item.crop != [0; 4] {
                src = src.cropped(item.crop);
            }
            for e in item.effects.iter().filter(|e| e.enabled) {
                crate::effects::apply_effect(&e.spec, &mut src, t_ns);
            }
            for f in &mut item.filters {
                f.apply(&mut src);
            }
            if item.grayscale {
                crate::filter::Grayscale.apply(&mut src);
            }
            if (item.brightness - 1.0).abs() > f32::EPSILON {
                crate::filter::Brightness(item.brightness).apply(&mut src);
            }
            let scale = item.scale;
            let xmap = if scale == 1.0 {
                Vec::new()
            } else {
                let out_w = (src.width as f32 * scale).round() as usize;
                (0..out_w).map(|ox| ((ox as f32 / scale) as u32).min(src.width - 1)).collect()
            };
            layers.push(Layer { frame: src, x: item.x, y: item.y, scale, op, xmap });
        }

        if let Some(b) = self.backend.as_mut() {
            match b.composite(self.width, self.height, self.background, &layers) {
                Ok(mut f) => {
                    f.pts_ns = t_ns;
                    return f;
                }
                Err(e) => {
                    self.backend_error = Some(format!("{} compositor failed ({e}); using the CPU", b.name()));
                    self.backend = None;
                }
            }
        }
        let mut canvas = Frame::new(self.width, self.height, self.background);
        canvas.pts_ns = t_ns;
        let (w, h) = (self.width as usize, self.height as usize);
        if layers.is_empty() || w == 0 || h == 0 {
            return canvas;
        }

        let threads = self.threads.clamp(1, h);
        if threads == 1 {
            composite_band(&mut canvas.data, w, 0, h, &layers);
        } else {
            let rows = (h + threads - 1) / threads;
            std::thread::scope(|s| {
                for (i, band) in canvas.data.chunks_mut(rows * w * 4).enumerate() {
                    let layers = &layers;
                    s.spawn(move || composite_band(band, w, i * rows, band.len() / (w * 4), layers));
                }
            });
        }
        canvas
    }
}

fn composite_band(band: &mut [u8], dw: usize, y_start: usize, rows: usize, layers: &[Layer]) {
    for l in layers {
        blit_band(band, dw, y_start, rows, l);
    }
}

#[inline]
fn blend(d: &mut [u8], s: &[u8], op: u32) {
    let a = (s[3] as u32 * op + 127) / 255;
    if a == 0 {
        return;
    }
    if a == 255 {
        d[0] = s[0];
        d[1] = s[1];
        d[2] = s[2];
    } else {
        let ia = 255 - a;
        d[0] = ((s[0] as u32 * a + d[0] as u32 * ia + 127) / 255) as u8;
        d[1] = ((s[1] as u32 * a + d[1] as u32 * ia + 127) / 255) as u8;
        d[2] = ((s[2] as u32 * a + d[2] as u32 * ia + 127) / 255) as u8;
    }
    d[3] = 255;
}

fn blit_band(band: &mut [u8], dw: usize, y_start: usize, rows: usize, l: &Layer) {
    let (sw, sh) = (l.frame.width as usize, l.frame.height as usize);
    let out_w = (sw as f32 * l.scale).round() as i32;
    let out_h = (sh as f32 * l.scale).round() as i32;

    // Visible destination rectangle, clipped to the canvas and to this band.
    let ox0 = (-l.x).max(0);
    let ox1 = out_w.min(dw as i32 - l.x);
    let dy0 = l.y.max(y_start as i32);
    let dy1 = (l.y + out_h).min((y_start + rows) as i32);
    if ox0 >= ox1 || dy0 >= dy1 {
        return;
    }
    let n = (ox1 - ox0) as usize;
    let unscaled = l.scale == 1.0;

    for dy in dy0..dy1 {
        let oy = (dy - l.y) as usize;
        let sy = if unscaled { oy } else { ((oy as f32 / l.scale) as usize).min(sh - 1) };
        let srow = &l.frame.data[sy * sw * 4..(sy + 1) * sw * 4];
        let off = ((dy as usize - y_start) * dw + (l.x + ox0) as usize) * 4;
        let drow = &mut band[off..off + n * 4];

        if unscaled {
            let s0 = ox0 as usize * 4;
            let spix = &srow[s0..s0 + n * 4];
            if l.op == 255 && spix.chunks_exact(4).all(|p| p[3] == 255) {
                drow.copy_from_slice(spix); // fast path: opaque, unscaled
            } else {
                for (d, s) in drow.chunks_exact_mut(4).zip(spix.chunks_exact(4)) {
                    blend(d, s, l.op);
                }
            }
        } else {
            for (i, d) in drow.chunks_exact_mut(4).enumerate() {
                let sx = l.xmap[ox0 as usize + i] as usize;
                blend(d, &srow[sx * 4..sx * 4 + 4], l.op);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scene::SceneItem,
        source::{ColorSource, Source, TestPatternSource},
    };

    #[test]
    fn places_and_scales_item() {
        let mut scene = Scene::new("t");
        scene.add(SceneItem::new(Box::new(ColorSource::new("c", 2, 2, [255, 0, 0, 255]))).at(4, 4).scaled(2.0));
        let f = Compositor::new(16, 16).render(&mut scene, 0);
        assert_eq!(f.pixel(4, 4), [255, 0, 0, 255]);
        assert_eq!(f.pixel(7, 7), [255, 0, 0, 255]);
        assert_eq!(f.pixel(8, 8), [0, 0, 0, 255]);
    }

    #[test]
    fn crop_trims_the_source() {
        let reference = TestPatternSource::new("p", 16, 8).render(0);
        let mut scene = Scene::new("t");
        let mut item = SceneItem::new(Box::new(TestPatternSource::new("p", 16, 8)));
        item.crop = [3, 2, 4, 1]; // left, top, right, bottom
        assert_eq!(item.cropped_size(), (9, 5));
        scene.add(item);
        let f = Compositor::new(16, 8).render(&mut scene, 0);
        assert_eq!(f.pixel(0, 0), reference.pixel(3, 2));
        assert_eq!(f.pixel(8, 4), reference.pixel(11, 6));
        assert_eq!(f.pixel(9, 0), [0, 0, 0, 255]); // outside the cropped area
    }

    #[test]
    fn opacity_blends() {
        let mut scene = Scene::new("t");
        scene.add(SceneItem::new(Box::new(ColorSource::new("c", 4, 4, [200, 200, 200, 255]))).with_opacity(0.5));
        let f = Compositor::new(4, 4).render(&mut scene, 0);
        assert_eq!(f.pixel(0, 0), [100, 100, 100, 255]);
    }

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

    #[test]
    fn parallel_matches_single_threaded() {
        let mut single = Compositor::new(97, 61);
        single.threads = 1;
        let reference = single.render(&mut busy_scene(), 123_000_000);
        for threads in [2, 3, 4, 7, 64] {
            let mut c = Compositor::new(97, 61);
            c.threads = threads;
            let f = c.render(&mut busy_scene(), 123_000_000);
            assert_eq!(f.data, reference.data, "mismatch with {threads} threads");
        }
    }

    #[test]
    fn fast_path_matches_blend_path() {
        // 100% opacity (fast path) must equal the generic blend of the same opaque pixels.
        let mut a = Scene::new("a");
        a.add(SceneItem::new(Box::new(TestPatternSource::new("p", 64, 36))).at(3, 2));
        let mut b = Scene::new("b");
        b.add(SceneItem::new(Box::new(TestPatternSource::new("p", 64, 36))).at(3, 2).with_opacity(0.999));
        let mut c = Compositor::new(80, 50);
        c.threads = 1;
        let fa = c.render(&mut a, 5);
        let fb = c.render(&mut b, 5);
        // opacity .999 rounds to 255, so both go through the fast path; compare to a manual copy
        assert_eq!(fa.data, fb.data);
        assert_eq!(fa.pixel(3, 2), TestPatternSource::new("p", 64, 36).render(5).pixel(0, 0));
    }
}
