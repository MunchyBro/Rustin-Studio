use crate::{effects::Effect, filter::Filter, source::Source};

pub struct SceneItem {
    pub source: Box<dyn Source>,
    pub filters: Vec<Box<dyn Filter>>,
    /// Top-left position on the canvas.
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    pub opacity: f32,
    pub visible: bool,
    /// Built-in per-item effects, editable from the GUI.
    pub grayscale: bool,
    pub brightness: f32,
    /// Pixels trimmed from the source: [left, top, right, bottom].
    pub crop: [u32; 4],
    /// Ordered filters (chroma key, blur, ...), applied after cropping.
    pub effects: Vec<Effect>,
}

impl SceneItem {
    pub fn new(source: Box<dyn Source>) -> Self {
        Self { source, filters: Vec::new(), x: 0, y: 0, scale: 1.0, opacity: 1.0, visible: true, grayscale: false, brightness: 1.0, crop: [0; 4], effects: Vec::new() }
    }
    /// Source size after cropping (never smaller than 1x1).
    pub fn cropped_size(&self) -> (u32, u32) {
        let (w, h) = self.source.size();
        (
            w.saturating_sub(self.crop[0].saturating_add(self.crop[2])).max(1),
            h.saturating_sub(self.crop[1].saturating_add(self.crop[3])).max(1),
        )
    }
    pub fn at(mut self, x: i32, y: i32) -> Self { self.x = x; self.y = y; self }
    pub fn scaled(mut self, s: f32) -> Self { self.scale = s; self }
    pub fn with_opacity(mut self, o: f32) -> Self { self.opacity = o; self }
    pub fn with_filter(mut self, f: Box<dyn Filter>) -> Self { self.filters.push(f); self }
}

/// Items are drawn bottom-to-top in vector order.
pub struct Scene {
    pub name: String,
    pub items: Vec<SceneItem>,
}

impl Scene {
    pub fn new(name: &str) -> Self {
        Self { name: name.into(), items: Vec::new() }
    }
    pub fn add(&mut self, item: SceneItem) -> &mut Self {
        self.items.push(item);
        self
    }
}

#[derive(Default)]
pub struct SceneCollection {
    pub scenes: Vec<Scene>,
    pub active: usize,
}

impl SceneCollection {
    pub fn add_scene(&mut self, scene: Scene) -> usize {
        self.scenes.push(scene);
        self.scenes.len() - 1
    }
    pub fn find(&self, name: &str) -> Option<usize> {
        self.scenes.iter().position(|s| s.name == name)
    }
    pub fn switch_to(&mut self, name: &str) -> bool {
        match self.find(name) {
            Some(i) => { self.active = i; true }
            None => false,
        }
    }
}
