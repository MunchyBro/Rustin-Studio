use crate::frame::Frame;
use serde::{Deserialize, Serialize};

/// Serializable description of a source: enough to rebuild it on the next launch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SourceSpec {
    Color { name: String, rgba: [u8; 4], width: u32, height: u32 },
    TestPattern { name: String, width: u32, height: u32 },
    BouncingBox { name: String, width: u32, height: u32, box_size: u32, rgba: [u8; 4] },
    Display { name: String, monitor_name: String, width: u32, height: u32 },
    Window { name: String, app: String, title: String, width: u32, height: u32 },
    Image { name: String, path: String },
    Media { name: String, path: String, looping: bool, width: u32, height: u32 },
    Text { name: String, text: String, size: f32, rgba: [u8; 4], background: [u8; 4], font_path: String },
}

impl SourceSpec {
    pub fn name(&self) -> &str {
        match self {
            SourceSpec::Color { name, .. }
            | SourceSpec::TestPattern { name, .. }
            | SourceSpec::BouncingBox { name, .. }
            | SourceSpec::Display { name, .. }
            | SourceSpec::Window { name, .. }
            | SourceSpec::Image { name, .. }
            | SourceSpec::Media { name, .. }
            | SourceSpec::Text { name, .. } => name,
        }
    }

    pub fn with_name(mut self, new: &str) -> Self {
        match &mut self {
            SourceSpec::Color { name, .. }
            | SourceSpec::TestPattern { name, .. }
            | SourceSpec::BouncingBox { name, .. }
            | SourceSpec::Display { name, .. }
            | SourceSpec::Window { name, .. }
            | SourceSpec::Image { name, .. }
            | SourceSpec::Media { name, .. }
            | SourceSpec::Text { name, .. } => *name = new.to_string(),
        }
        self
    }
}

/// Anything that can produce video frames (capture, image, color, generator...).
pub trait Source: Send {
    fn name(&self) -> &str;
    fn set_name(&mut self, name: &str);
    /// Human-readable source type, e.g. "Display Capture".
    fn kind(&self) -> &'static str;
    /// Description used to save and restore this source.
    fn spec(&self) -> SourceSpec;
    /// Called every frame: true when this source is part of the live (program) scene and visible.
    /// Sources with audio use it to stay silent while they aren't being broadcast.
    fn set_live(&mut self, _live: bool) {}
    /// Text sources expose themselves so the properties dialog can edit them.
    fn edit_text(&mut self) -> Option<&mut crate::text::TextSource> {
        None
    }
    /// Native size of the source in pixels.
    fn size(&self) -> (u32, u32);
    /// Render the source at time `t_ns` (nanoseconds since the scene started).
    fn render(&mut self, t_ns: u64) -> Frame;
}

/// Solid color source.
pub struct ColorSource {
    name: String,
    w: u32,
    h: u32,
    rgba: [u8; 4],
}

impl ColorSource {
    pub fn new(name: &str, w: u32, h: u32, rgba: [u8; 4]) -> Self {
        Self { name: name.into(), w, h, rgba }
    }
}

impl Source for ColorSource {
    fn name(&self) -> &str { &self.name }
    fn set_name(&mut self, name: &str) { self.name = name.into(); }
    fn kind(&self) -> &'static str { "Color Source" }
    fn spec(&self) -> SourceSpec { SourceSpec::Color { name: self.name.clone(), rgba: self.rgba, width: self.w, height: self.h } }
    fn size(&self) -> (u32, u32) { (self.w, self.h) }
    fn render(&mut self, t_ns: u64) -> Frame {
        let mut f = Frame::new(self.w, self.h, self.rgba);
        f.pts_ns = t_ns;
        f
    }
}

/// Animated gradient with a sweeping bar: handy for verifying the pipeline.
pub struct TestPatternSource {
    name: String,
    w: u32,
    h: u32,
}

impl TestPatternSource {
    pub fn new(name: &str, w: u32, h: u32) -> Self {
        Self { name: name.into(), w, h }
    }
}

impl Source for TestPatternSource {
    fn name(&self) -> &str { &self.name }
    fn set_name(&mut self, name: &str) { self.name = name.into(); }
    fn kind(&self) -> &'static str { "Test Pattern" }
    fn spec(&self) -> SourceSpec { SourceSpec::TestPattern { name: self.name.clone(), width: self.w, height: self.h } }
    fn size(&self) -> (u32, u32) { (self.w, self.h) }
    fn render(&mut self, t_ns: u64) -> Frame {
        let secs = t_ns as f64 / 1e9;
        let mut f = Frame::transparent(self.w, self.h);
        f.pts_ns = t_ns;
        let bar_w = (self.w / 16).max(1);
        let bar_x = (((secs * 0.5).fract()) * self.w as f64) as u32;
        let shift = (secs * 40.0) as u32;
        for y in 0..self.h {
            for x in 0..self.w {
                let r = (((x + shift) % self.w.max(1)) * 255 / self.w.max(1)) as u8;
                let g = ((y * 255) / self.h.max(1)) as u8;
                let in_bar = x >= bar_x && x < bar_x + bar_w;
                let p = if in_bar { [255, 255, 255, 255] } else { [r, g, 128, 255] };
                f.set_pixel(x, y, p);
            }
        }
        f
    }
}

/// A box that bounces around inside a transparent canvas (useful as an overlay).
pub struct BouncingBoxSource {
    name: String,
    w: u32,
    h: u32,
    box_size: u32,
    rgba: [u8; 4],
}

impl BouncingBoxSource {
    pub fn new(name: &str, w: u32, h: u32, box_size: u32, rgba: [u8; 4]) -> Self {
        Self { name: name.into(), w, h, box_size, rgba }
    }

    fn bounce(t: f64, range: u32, speed: f64) -> u32 {
        if range == 0 { return 0; }
        let period = 2.0 * range as f64;
        let p = (t * speed) % period;
        (if p < range as f64 { p } else { period - p }) as u32
    }
}

impl Source for BouncingBoxSource {
    fn name(&self) -> &str { &self.name }
    fn set_name(&mut self, name: &str) { self.name = name.into(); }
    fn kind(&self) -> &'static str { "Bouncing Box" }
    fn spec(&self) -> SourceSpec { SourceSpec::BouncingBox { name: self.name.clone(), width: self.w, height: self.h, box_size: self.box_size, rgba: self.rgba } }
    fn size(&self) -> (u32, u32) { (self.w, self.h) }
    fn render(&mut self, t_ns: u64) -> Frame {
        let t = t_ns as f64 / 1e9;
        let mut f = Frame::transparent(self.w, self.h);
        f.pts_ns = t_ns;
        let bx = Self::bounce(t, self.w.saturating_sub(self.box_size), 220.0);
        let by = Self::bounce(t, self.h.saturating_sub(self.box_size), 160.0);
        for y in by..(by + self.box_size).min(self.h) {
            for x in bx..(bx + self.box_size).min(self.w) {
                f.set_pixel(x, y, self.rgba);
            }
        }
        f
    }
}
