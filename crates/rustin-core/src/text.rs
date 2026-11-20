//! Text source: renders (multi-line) text to a transparent RGBA frame with ab_glyph.

use crate::{
    frame::Frame,
    source::{Source, SourceSpec},
};
use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};

/// Ubuntu Light, bundled under the Ubuntu Font Licence 1.0 (see assets/UFL.txt).
const BUNDLED_FONT: &[u8] = include_bytes!("../assets/Ubuntu-Light.ttf");
const MAX_DIM: u32 = 4096;

pub struct TextSource {
    name: String,
    pub text: String,
    /// Font size in pixels.
    pub size: f32,
    pub color: [u8; 4],
    /// Background fill; alpha 0 = transparent.
    pub background: [u8; 4],
    /// Optional path to a .ttf/.otf; empty or unreadable falls back to the bundled font.
    pub font_path: String,
    font: FontArc,
    loaded_font_path: String,
    frame: Frame,
}

fn bundled() -> FontArc {
    FontArc::try_from_slice(BUNDLED_FONT).expect("bundled font is valid")
}

impl TextSource {
    pub fn new(name: &str, text: &str, size: f32, color: [u8; 4]) -> Self {
        let mut t = Self {
            name: name.into(),
            text: text.into(),
            size,
            color,
            background: [0, 0, 0, 0],
            font_path: String::new(),
            font: bundled(),
            loaded_font_path: String::new(),
            frame: Frame::transparent(1, 1),
        };
        t.update();
        t
    }

    /// Re-render after changing `text`, `size`, colors or `font_path`.
    pub fn update(&mut self) {
        if self.font_path != self.loaded_font_path {
            self.font = std::fs::read(&self.font_path)
                .ok()
                .and_then(|bytes| FontArc::try_from_vec(bytes).ok())
                .unwrap_or_else(bundled);
            self.loaded_font_path = self.font_path.clone();
        }
        self.size = self.size.clamp(6.0, 600.0);
        self.frame = render_text(&self.text, self.size, self.color, self.background, &self.font);
    }

    /// True if `font_path` was set but couldn't be loaded.
    pub fn font_failed(&self) -> bool {
        !self.font_path.is_empty() && std::fs::read(&self.font_path).ok().and_then(|b| FontArc::try_from_vec(b).ok()).is_none()
    }
}

fn render_text(text: &str, size: f32, color: [u8; 4], bg: [u8; 4], font: &FontArc) -> Frame {
    let scaled = font.as_scaled(PxScale::from(size));
    let line_h = scaled.ascent() - scaled.descent() + scaled.line_gap();
    let pad = (size * 0.15).round();
    let lines: Vec<&str> = text.split('\n').collect();

    let advance = |line: &str| -> f32 {
        let mut x = 0.0;
        let mut prev = None;
        for c in line.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            x += scaled.h_advance(id);
            prev = Some(id);
        }
        x
    };
    let max_w = lines.iter().map(|l| advance(l)).fold(0.0f32, f32::max);
    let w = ((max_w + 2.0 * pad).ceil() as u32).clamp(1, MAX_DIM);
    let h = ((line_h * lines.len() as f32 + 2.0 * pad).ceil() as u32).clamp(1, MAX_DIM);
    let mut frame = Frame::new(w, h, bg);

    for (li, line) in lines.iter().enumerate() {
        let baseline = pad + li as f32 * line_h + scaled.ascent();
        let mut x = pad;
        let mut prev = None;
        for c in line.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(size), point(x, baseline));
            x += scaled.h_advance(id);
            prev = Some(id);
            if let Some(outlined) = font.outline_glyph(glyph) {
                let b = outlined.px_bounds();
                outlined.draw(|gx, gy, cov| {
                    let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 || cov <= 0.0 {
                        return;
                    }
                    let d = frame.pixel(px as u32, py as u32);
                    // Straight-alpha "over".
                    let sa = cov * color[3] as f32 / 255.0;
                    let da = d[3] as f32 / 255.0;
                    let oa = sa + da * (1.0 - sa);
                    if oa <= 0.0 {
                        return;
                    }
                    let mix = |s: u8, dc: u8| ((s as f32 * sa + dc as f32 * da * (1.0 - sa)) / oa).round() as u8;
                    frame.set_pixel(px as u32, py as u32, [mix(color[0], d[0]), mix(color[1], d[1]), mix(color[2], d[2]), (oa * 255.0).round() as u8]);
                });
            }
        }
    }
    frame
}

impl Source for TextSource {
    fn name(&self) -> &str { &self.name }
    fn set_name(&mut self, name: &str) { self.name = name.into(); }
    fn kind(&self) -> &'static str { "Text" }
    fn spec(&self) -> SourceSpec {
        SourceSpec::Text {
            name: self.name.clone(),
            text: self.text.clone(),
            size: self.size,
            rgba: self.color,
            background: self.background,
            font_path: self.font_path.clone(),
        }
    }
    fn size(&self) -> (u32, u32) { (self.frame.width, self.frame.height) }
    fn render(&mut self, t_ns: u64) -> Frame {
        let mut f = self.frame.clone();
        f.pts_ns = t_ns;
        f
    }
    fn edit_text(&mut self) -> Option<&mut TextSource> { Some(self) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque_pixels(f: &Frame) -> usize { f.data.chunks_exact(4).filter(|p| p[3] > 0).count() }

    #[test]
    fn renders_visible_text_in_the_requested_color() {
        let t = TextSource::new("t", "Hello", 48.0, [255, 0, 0, 255]);
        let f = &t.frame;
        assert!(opaque_pixels(f) > 100);
        // Fully covered pixels carry the text color.
        let solid = f.data.chunks_exact(4).find(|p| p[3] == 255).expect("a solid pixel");
        assert_eq!(&solid[..3], &[255, 0, 0]);
    }

    #[test]
    fn longer_and_multiline_text_is_bigger() {
        let one = TextSource::new("a", "Hi", 40.0, [255; 4]).size();
        let wide = TextSource::new("b", "Hi there, friend", 40.0, [255; 4]).size();
        let tall = TextSource::new("c", "Hi\nthere", 40.0, [255; 4]).size();
        assert!(wide.0 > one.0 * 3);
        assert!(tall.1 > one.1 + 30);
    }

    #[test]
    fn update_picks_up_changes_and_bad_font_falls_back() {
        let mut t = TextSource::new("t", "A", 30.0, [255; 4]);
        let before = t.size();
        t.text = "AAAAAA".into();
        t.font_path = "/definitely/not/a/font.ttf".into();
        t.update();
        assert!(t.size().0 > before.0 * 3);
        assert!(t.font_failed());
        assert!(opaque_pixels(&t.frame) > 0); // still rendered with the bundled font
    }

    #[test]
    fn background_fills_the_box() {
        let mut t = TextSource::new("t", "x", 30.0, [255; 4]);
        t.background = [10, 20, 30, 255];
        t.update();
        assert_eq!(t.frame.pixel(0, 0), [10, 20, 30, 255]);
    }
}
