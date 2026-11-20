//! Per-source video effects: serializable specs applied in order after cropping.

use crate::frame::Frame;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaskMode {
    /// Use the mask image's own alpha channel.
    Alpha,
    /// White = visible, black = hidden.
    Luma,
    /// Black = visible, white = hidden.
    InvertedLuma,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FilterSpec {
    /// Make a color transparent (green/blue screen). Distances are measured on chroma
    /// (Cb/Cr) only, so shading on the screen doesn't matter.
    ChromaKey { key: [u8; 3], similarity: f32, smoothness: f32, spill: f32 },
    /// Soft blur; `radius` in pixels.
    Blur { radius: u32 },
    /// brightness -1..1 (0 neutral), contrast 0..3 (1 neutral), saturation 0..3 (1 neutral),
    /// gamma 0.3..3 (1 neutral).
    ColorCorrection { brightness: f32, contrast: f32, saturation: f32, gamma: f32 },
    /// Unsharp mask; `amount` 0..3.
    Sharpen { amount: f32 },
    /// Apply a 3D color LUT from a `.cube` file; `amount` 0..1 blends with the original.
    Lut { path: String, amount: f32 },
    /// Hide parts of the source using an image (PNG/JPEG), stretched to the source size.
    Mask { path: String, mode: MaskMode },
    /// Endless wrap-around scrolling, in source pixels per second (negative = other direction).
    Scroll { x: f32, y: f32 },
    Flip { horizontal: bool, vertical: bool },
}

impl FilterSpec {
    pub fn label(&self) -> &'static str {
        match self {
            FilterSpec::ChromaKey { .. } => "Chroma Key",
            FilterSpec::Blur { .. } => "Blur",
            FilterSpec::ColorCorrection { .. } => "Color Correction",
            FilterSpec::Sharpen { .. } => "Sharpen",
            FilterSpec::Lut { .. } => "Color LUT",
            FilterSpec::Mask { .. } => "Image Mask",
            FilterSpec::Scroll { .. } => "Scroll",
            FilterSpec::Flip { .. } => "Flip",
        }
    }

    pub fn chroma_key() -> Self {
        FilterSpec::ChromaKey { key: [0, 255, 0], similarity: 0.30, smoothness: 0.10, spill: 0.5 }
    }
    pub fn blur() -> Self { FilterSpec::Blur { radius: 6 } }
    pub fn color_correction() -> Self {
        FilterSpec::ColorCorrection { brightness: 0.0, contrast: 1.0, saturation: 1.0, gamma: 1.0 }
    }
    pub fn sharpen() -> Self { FilterSpec::Sharpen { amount: 0.8 } }
    pub fn lut() -> Self { FilterSpec::Lut { path: String::new(), amount: 1.0 } }
    pub fn mask() -> Self { FilterSpec::Mask { path: String::new(), mode: MaskMode::Luma } }
    pub fn scroll() -> Self { FilterSpec::Scroll { x: 80.0, y: 0.0 } }
    pub fn flip() -> Self { FilterSpec::Flip { horizontal: true, vertical: false } }
}

/// A filter in a source's list, which can be switched off without removing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub enabled: bool,
    pub spec: FilterSpec,
}

impl Effect {
    pub fn new(spec: FilterSpec) -> Self { Self { enabled: true, spec } }
}

/// Apply one effect. `t_ns` is the scene clock, used by time-based effects like Scroll.
pub fn apply_effect(spec: &FilterSpec, frame: &mut Frame, t_ns: u64) {
    match spec {
        FilterSpec::ChromaKey { key, similarity, smoothness, spill } => chroma_key(frame, *key, *similarity, *smoothness, *spill),
        FilterSpec::Blur { radius } => blur(frame, *radius),
        FilterSpec::ColorCorrection { brightness, contrast, saturation, gamma } => {
            color_correction(frame, *brightness, *contrast, *saturation, *gamma)
        }
        FilterSpec::Sharpen { amount } => sharpen(frame, *amount),
        FilterSpec::Lut { path, amount } => lut(frame, path, *amount),
        FilterSpec::Mask { path, mode } => mask(frame, path, *mode),
        FilterSpec::Scroll { x, y } => scroll(frame, *x, *y, t_ns),
        FilterSpec::Flip { horizontal, vertical } => flip(frame, *horizontal, *vertical),
    }
}

// ------------------------------------------------------------------ chroma key

#[inline]
fn cbcr(r: f32, g: f32, b: f32) -> (f32, f32) {
    (128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b, 128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b)
}

fn chroma_key(frame: &mut Frame, key: [u8; 3], similarity: f32, smoothness: f32, spill: f32) {
    let (kcb, kcr) = cbcr(key[0] as f32, key[1] as f32, key[2] as f32);
    let smooth = smoothness.max(1e-3);
    let spill = spill.clamp(0.0, 1.0);
    for px in frame.data.chunks_exact_mut(4) {
        let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
        let (cb, cr) = cbcr(r, g, b);
        let d = ((cb - kcb).powi(2) + (cr - kcr).powi(2)).sqrt() / 255.0;
        // 0 = fully keyed out, 1 = fully kept.
        let keep = ((d - similarity) / smooth).clamp(0.0, 1.0);
        px[3] = (px[3] as f32 * keep).round() as u8;
        if keep < 1.0 && spill > 0.0 && keep > 0.0 {
            // Edge pixels: pull color toward gray to remove the screen-colored fringe.
            let luma = 0.299 * r + 0.587 * g + 0.114 * b;
            let k = spill * (1.0 - keep);
            px[0] = (r + (luma - r) * k).round() as u8;
            px[1] = (g + (luma - g) * k).round() as u8;
            px[2] = (b + (luma - b) * k).round() as u8;
        }
    }
}

// ------------------------------------------------------------------ color correction

fn color_correction(frame: &mut Frame, brightness: f32, contrast: f32, saturation: f32, gamma: f32) {
    let inv_gamma = 1.0 / gamma.clamp(0.1, 5.0);
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        let x = (i as f32 / 255.0).powf(inv_gamma);
        let x = (x - 0.5) * contrast + 0.5 + brightness;
        *v = (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    let sat = saturation.max(0.0);
    let neutral_sat = (sat - 1.0).abs() < 1e-4;
    for px in frame.data.chunks_exact_mut(4) {
        let (mut r, mut g, mut b) = (px[0], px[1], px[2]);
        if !neutral_sat {
            let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
            let s = |c: u8| (luma + (c as f32 - luma) * sat).clamp(0.0, 255.0) as u8;
            (r, g, b) = (s(r), s(g), s(b));
        }
        px[0] = lut[r as usize];
        px[1] = lut[g as usize];
        px[2] = lut[b as usize];
    }
}

// ------------------------------------------------------------------ blur / sharpen

/// One box-blur pass along rows (`horizontal`) or columns, on premultiplied RGBA in u16-safe sums.
fn box_pass(src: &[[u32; 4]], dst: &mut [[u32; 4]], w: usize, h: usize, r: usize, horizontal: bool) {
    let (len, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let win = (2 * r + 1) as u32;
    for line in 0..lines {
        let mut acc = [0u32; 4];
        // Prime the window for position 0 (edge pixels are clamped/replicated).
        for k in 0..=2 * r {
            let i = k.saturating_sub(r).min(len - 1);
            let p = src[idx(line, i)];
            for c in 0..4 { acc[c] += p[c]; }
        }
        for i in 0..len {
            let o = &mut dst[idx(line, i)];
            for c in 0..4 { o[c] = (acc[c] + win / 2) / win; }
            let add = (i + r + 1).min(len - 1);
            let sub = i.saturating_sub(r);
            let (pa, ps) = (src[idx(line, add)], src[idx(line, sub)]);
            for c in 0..4 { acc[c] = acc[c] + pa[c] - ps[c]; }
        }
    }
}

fn to_premul(frame: &Frame) -> Vec<[u32; 4]> {
    frame
        .data
        .chunks_exact(4)
        .map(|p| {
            let a = p[3] as u32;
            [(p[0] as u32 * a + 127) / 255, (p[1] as u32 * a + 127) / 255, (p[2] as u32 * a + 127) / 255, a]
        })
        .collect()
}

fn from_premul(frame: &mut Frame, px: &[[u32; 4]]) {
    for (o, p) in frame.data.chunks_exact_mut(4).zip(px) {
        let a = p[3];
        if a == 0 {
            o.copy_from_slice(&[0, 0, 0, 0]);
        } else {
            for c in 0..3 { o[c] = ((p[c] * 255 + a / 2) / a).min(255) as u8; }
            o[3] = a as u8;
        }
    }
}

fn blur(frame: &mut Frame, radius: u32) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let r = (radius as usize).min(w.max(h));
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut a = to_premul(frame);
    let mut b = a.clone();
    // Two box passes per axis approximate a Gaussian.
    for _ in 0..2 {
        box_pass(&a, &mut b, w, h, r, true);
        box_pass(&b, &mut a, w, h, r, false);
    }
    from_premul(frame, &a);
}

fn sharpen(frame: &mut Frame, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let mut soft = frame.clone();
    blur(&mut soft, 1);
    for (o, s) in frame.data.chunks_exact_mut(4).zip(soft.data.chunks_exact(4)) {
        for c in 0..3 {
            let v = o[c] as f32 + amount * (o[c] as f32 - s[c] as f32);
            o[c] = v.clamp(0.0, 255.0).round() as u8;
        }
    }
}

// ------------------------------------------------------------------ file caches

type Cache<T> = Mutex<HashMap<String, (Option<SystemTime>, Option<Arc<T>>)>>;

/// Load a file-backed resource once per modification time (failures are cached too, so a bad
/// path isn't re-read every frame).
fn cached<T>(cache: &'static OnceLock<Cache<T>>, path: &str, load: impl Fn(&str) -> Option<T>) -> Option<Arc<T>> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut g = cache.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    if let Some((m, v)) = g.get(path) {
        if *m == mtime {
            return v.clone();
        }
    }
    let v = load(path).map(Arc::new);
    g.insert(path.to_string(), (mtime, v.clone()));
    v
}

// ------------------------------------------------------------------ 3D LUT (.cube)

/// A 3D color lookup table. `data` is indexed `r + g*N + b*N*N` (red varies fastest).
pub struct Lut {
    size: usize,
    data: Vec<[f32; 3]>,
    dmin: [f32; 3],
    dmax: [f32; 3],
}

impl Lut {
    pub fn parse_cube(text: &str) -> Result<Lut, String> {
        let (mut size, mut dmin, mut dmax) = (0usize, [0f32; 3], [1f32; 3]);
        let mut data: Vec<[f32; 3]> = Vec::new();
        let triple = |it: std::str::SplitWhitespace| -> Option<[f32; 3]> {
            let v: Vec<f32> = it.filter_map(|t| t.parse().ok()).collect();
            (v.len() == 3).then(|| [v[0], v[1], v[2]])
        };
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut it = l.split_whitespace();
            match it.next().unwrap() {
                "TITLE" => {}
                "LUT_3D_SIZE" => size = it.next().and_then(|v| v.parse().ok()).ok_or("bad LUT_3D_SIZE")?,
                "LUT_1D_SIZE" => return Err("1D LUTs aren't supported (use a 3D .cube)".into()),
                "DOMAIN_MIN" => dmin = triple(it).ok_or("bad DOMAIN_MIN")?,
                "DOMAIN_MAX" => dmax = triple(it).ok_or("bad DOMAIN_MAX")?,
                first => {
                    let row = triple(std::iter::once(first).chain(it).collect::<Vec<_>>().join(" ").split_whitespace());
                    data.push(row.ok_or_else(|| format!("bad LUT row: {l}"))?);
                }
            }
        }
        if size < 2 || data.len() != size * size * size {
            return Err(format!("expected {} LUT entries, found {}", size * size * size, data.len()));
        }
        Ok(Lut { size, data, dmin, dmax })
    }

    /// Trilinear lookup; input and output are 0..1.
    pub fn sample(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let m = (n - 1) as f32;
        let mut i0 = [0usize; 3];
        let mut i1 = [0usize; 3];
        let mut f = [0f32; 3];
        for c in 0..3 {
            let range = (self.dmax[c] - self.dmin[c]).max(1e-6);
            let t = ((rgb[c] - self.dmin[c]) / range).clamp(0.0, 1.0) * m;
            i0[c] = (t.floor() as usize).min(n - 1);
            i1[c] = (i0[c] + 1).min(n - 1);
            f[c] = t - i0[c] as f32;
        }
        let at = |r: usize, g: usize, b: usize| self.data[r + g * n + b * n * n];
        let mut out = [0f32; 3];
        for c in 0..3 {
            let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
            let c00 = lerp(at(i0[0], i0[1], i0[2])[c], at(i1[0], i0[1], i0[2])[c], f[0]);
            let c10 = lerp(at(i0[0], i1[1], i0[2])[c], at(i1[0], i1[1], i0[2])[c], f[0]);
            let c01 = lerp(at(i0[0], i0[1], i1[2])[c], at(i1[0], i0[1], i1[2])[c], f[0]);
            let c11 = lerp(at(i0[0], i1[1], i1[2])[c], at(i1[0], i1[1], i1[2])[c], f[0]);
            out[c] = lerp(lerp(c00, c10, f[1]), lerp(c01, c11, f[1]), f[2]);
        }
        out
    }
}

static LUTS: OnceLock<Cache<Lut>> = OnceLock::new();

fn lut(frame: &mut Frame, path: &str, amount: f32) {
    let Some(l) = cached(&LUTS, path, |p| std::fs::read_to_string(p).ok().and_then(|t| Lut::parse_cube(&t).ok())) else { return };
    let amount = amount.clamp(0.0, 1.0);
    if amount <= 0.0 {
        return;
    }
    for px in frame.data.chunks_exact_mut(4) {
        let src = [px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0];
        let out = l.sample(src);
        for c in 0..3 {
            px[c] = ((src[c] + (out[c] - src[c]) * amount) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

// ------------------------------------------------------------------ image mask

struct MaskImg {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

static MASKS: OnceLock<Cache<MaskImg>> = OnceLock::new();

fn mask(frame: &mut Frame, path: &str, mode: MaskMode) {
    let Some(m) = cached(&MASKS, path, |p| {
        let img = image::open(p).ok()?.to_rgba8();
        Some(MaskImg { w: img.width(), h: img.height(), data: img.into_raw() })
    }) else {
        return;
    };
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    for y in 0..fh {
        let my = y * m.h as usize / fh;
        for x in 0..fw {
            let mx = x * m.w as usize / fw;
            let p = &m.data[(my * m.w as usize + mx) * 4..][..4];
            let luma = (77 * p[0] as u32 + 150 * p[1] as u32 + 29 * p[2] as u32) >> 8;
            let k = match mode {
                MaskMode::Alpha => p[3] as u32,
                MaskMode::Luma => luma,
                MaskMode::InvertedLuma => 255 - luma,
            };
            let a = &mut frame.data[(y * fw + x) * 4 + 3];
            *a = ((*a as u32 * k + 127) / 255) as u8;
        }
    }
}

// ------------------------------------------------------------------ scroll / flip

fn scroll(frame: &mut Frame, x: f32, y: f32, t_ns: u64) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let t = t_ns as f64 / 1e9;
    let dx = ((x as f64 * t).round() as i64).rem_euclid(w as i64) as usize;
    let dy = ((y as f64 * t).round() as i64).rem_euclid(h as i64) as usize;
    if dx != 0 {
        for row in frame.data.chunks_exact_mut(w * 4) {
            row.rotate_right(dx * 4);
        }
    }
    if dy != 0 {
        frame.data.rotate_right(dy * w * 4);
    }
}

fn flip(frame: &mut Frame, horizontal: bool, vertical: bool) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if horizontal {
        for row in frame.data.chunks_exact_mut(w * 4) {
            for i in 0..w / 2 {
                for c in 0..4 {
                    row.swap(i * 4 + c, (w - 1 - i) * 4 + c);
                }
            }
        }
    }
    if vertical {
        let row = w * 4;
        for y in 0..h / 2 {
            let (top, bottom) = frame.data.split_at_mut((h - 1 - y) * row);
            top[y * row..(y + 1) * row].swap_with_slice(&mut bottom[..row]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] { f.pixel(x, y) }

    #[test]
    fn chroma_key_removes_green_keeps_others() {
        let mut f = Frame::new(4, 1, [0, 255, 0, 255]);
        f.set_pixel(1, 0, [200, 30, 40, 255]); // red
        f.set_pixel(2, 0, [200, 150, 120, 255]); // skin tone
        f.set_pixel(3, 0, [0, 120, 0, 255]); // dark green: still the key color on chroma
        apply_effect(&FilterSpec::chroma_key(), &mut f, 0);
        assert_eq!(px(&f, 0, 0)[3], 0);
        assert_eq!(px(&f, 1, 0)[3], 255);
        assert_eq!(px(&f, 2, 0)[3], 255);
        assert_eq!(px(&f, 3, 0)[3], 0);
    }

    #[test]
    fn chroma_key_smoothness_gives_partial_alpha() {
        let mut f = Frame::new(1, 1, [60, 200, 60, 255]); // pale green, near the threshold
        apply_effect(&FilterSpec::ChromaKey { key: [0, 255, 0], similarity: 0.10, smoothness: 0.40, spill: 0.0 }, &mut f, 0);
        let a = px(&f, 0, 0)[3];
        assert!(a > 0 && a < 255, "alpha {a}");
    }

    #[test]
    fn blur_spreads_energy_and_preserves_flat_areas() {
        let mut f = Frame::new(9, 9, [0, 0, 0, 255]);
        f.set_pixel(4, 4, [255, 255, 255, 255]);
        apply_effect(&FilterSpec::Blur { radius: 2 }, &mut f, 0);
        let center = px(&f, 4, 4)[0];
        assert!(center < 255 && center > 0);
        assert!(px(&f, 3, 4)[0] > 0 && px(&f, 4, 2)[0] > 0);
        let mut flat = Frame::new(8, 8, [90, 60, 30, 255]);
        apply_effect(&FilterSpec::Blur { radius: 3 }, &mut flat, 0);
        assert!(flat.data.chunks_exact(4).all(|p| p == [90, 60, 30, 255]));
    }

    #[test]
    fn blur_keeps_transparent_edges_from_darkening() {
        // White opaque on the left half, fully transparent on the right.
        let mut f = Frame::transparent(8, 1);
        for x in 0..4 { f.set_pixel(x, 0, [255, 255, 255, 255]); }
        apply_effect(&FilterSpec::Blur { radius: 2 }, &mut f, 0);
        for x in 0..8 {
            let p = px(&f, x, 0);
            if p[3] > 0 { assert!(p[0] >= 250, "x={x} got {p:?}"); } // color stays white, only alpha fades
        }
    }

    #[test]
    fn color_correction_identity_and_saturation() {
        let orig = Frame::new(2, 1, [200, 100, 50, 255]);
        let mut same = orig.clone();
        apply_effect(&FilterSpec::color_correction(), &mut same, 0);
        assert_eq!(same.data, orig.data);
        let mut gray = orig.clone();
        apply_effect(&FilterSpec::ColorCorrection { brightness: 0.0, contrast: 1.0, saturation: 0.0, gamma: 1.0 }, &mut gray, 0);
        let p = px(&gray, 0, 0);
        assert!(p[0] == p[1] && p[1] == p[2]);
        let mut bright = orig.clone();
        apply_effect(&FilterSpec::ColorCorrection { brightness: 0.2, contrast: 1.0, saturation: 1.0, gamma: 1.0 }, &mut bright, 0);
        assert!(px(&bright, 0, 0)[2] > 50 + 40);
    }

    #[test]
    fn sharpen_overshoots_edges_but_not_flat_areas() {
        let mut f = Frame::new(8, 1, [100, 100, 100, 255]);
        for x in 4..8 { f.set_pixel(x, 0, [200, 200, 200, 255]); }
        apply_effect(&FilterSpec::Sharpen { amount: 1.0 }, &mut f, 0);
        assert!(px(&f, 3, 0)[0] < 100); // darker just before the edge
        assert!(px(&f, 4, 0)[0] > 200); // brighter just after it
        assert_eq!(px(&f, 0, 0)[0], 100);
        assert_eq!(px(&f, 7, 0)[0], 200);
    }

    #[test]
    fn specs_round_trip_through_json() {
        let list = vec![Effect::new(FilterSpec::chroma_key()), Effect { enabled: false, spec: FilterSpec::blur() }];
        let json = serde_json::to_string(&list).unwrap();
        let back: Vec<Effect> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, list);
    }

    fn ramp(n: u32) -> Frame {
        let mut f = Frame::new(n, 1, [0, 0, 0, 255]);
        for x in 0..n {
            f.set_pixel(x, 0, [10 + x as u8, 0, 0, 255]);
        }
        f
    }
    fn reds(f: &Frame) -> Vec<u8> { (0..f.width).map(|x| f.pixel(x, 0)[0]).collect() }

    #[test]
    fn scroll_wraps_with_time_and_direction() {
        let mut f = ramp(4);
        apply_effect(&FilterSpec::Scroll { x: 1.0, y: 0.0 }, &mut f, 1_000_000_000); // 1 px right
        assert_eq!(reds(&f), vec![13, 10, 11, 12]);
        let mut g = ramp(4);
        apply_effect(&FilterSpec::Scroll { x: -1.0, y: 0.0 }, &mut g, 1_000_000_000); // 1 px left
        assert_eq!(reds(&g), vec![11, 12, 13, 10]);
        let mut h = ramp(4);
        apply_effect(&FilterSpec::Scroll { x: 1.0, y: 0.0 }, &mut h, 4_000_000_000); // full wrap
        assert_eq!(reds(&h), vec![10, 11, 12, 13]);
        let mut v = Frame::new(1, 3, [0, 0, 0, 255]);
        for y in 0..3 { v.set_pixel(0, y, [y as u8 + 1, 0, 0, 255]); }
        apply_effect(&FilterSpec::Scroll { x: 0.0, y: 1.0 }, &mut v, 1_000_000_000);
        assert_eq!((0..3).map(|y| v.pixel(0, y)[0]).collect::<Vec<_>>(), vec![3, 1, 2]);
    }

    #[test]
    fn flip_horizontal_and_vertical() {
        let mut f = ramp(4);
        apply_effect(&FilterSpec::Flip { horizontal: true, vertical: false }, &mut f, 0);
        assert_eq!(reds(&f), vec![13, 12, 11, 10]);
        let mut v = Frame::new(1, 3, [0, 0, 0, 255]);
        for y in 0..3 { v.set_pixel(0, y, [y as u8 + 1, 0, 0, 255]); }
        apply_effect(&FilterSpec::Flip { horizontal: false, vertical: true }, &mut v, 0);
        assert_eq!((0..3).map(|y| v.pixel(0, y)[0]).collect::<Vec<_>>(), vec![3, 2, 1]);
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rustin-fx-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    /// Cube with 2 entries per axis; `f` maps each corner's (r,g,b) to its output.
    fn cube(f: impl Fn(f32, f32, f32) -> [f32; 3]) -> String {
        let mut s = String::from("TITLE \"test\"\nLUT_3D_SIZE 2\n");
        for b in 0..2 { for g in 0..2 { for r in 0..2 {
            let o = f(r as f32, g as f32, b as f32);
            s += &format!("{} {} {}\n", o[0], o[1], o[2]);
        }}}
        s
    }

    #[test]
    fn lut_identity_swap_and_amount() {
        let id = tmp("id.cube");
        std::fs::write(&id, cube(|r, g, b| [r, g, b])).unwrap();
        let mut f = Frame::new(1, 1, [200, 100, 50, 255]);
        apply_effect(&FilterSpec::Lut { path: id.to_string_lossy().into(), amount: 1.0 }, &mut f, 0);
        let p = f.pixel(0, 0);
        assert!((p[0] as i32 - 200).abs() <= 1 && (p[1] as i32 - 100).abs() <= 1 && (p[2] as i32 - 50).abs() <= 1, "{p:?}");

        let swap = tmp("swap.cube"); // red <-> blue
        std::fs::write(&swap, cube(|r, g, b| [b, g, r])).unwrap();
        let spec = |amount| FilterSpec::Lut { path: swap.to_string_lossy().into(), amount };
        let mut s = Frame::new(1, 1, [255, 0, 0, 255]);
        apply_effect(&spec(1.0), &mut s, 0);
        assert_eq!(s.pixel(0, 0), [0, 0, 255, 255]);
        let mut half = Frame::new(1, 1, [255, 0, 0, 255]);
        apply_effect(&spec(0.5), &mut half, 0);
        let p = half.pixel(0, 0);
        assert!((p[0] as i32 - 128).abs() <= 1 && (p[2] as i32 - 128).abs() <= 1, "{p:?}");
    }

    #[test]
    fn lut_parser_rejects_bad_files_and_missing_file_is_a_noop() {
        assert!(Lut::parse_cube("LUT_3D_SIZE 2\n0 0 0\n").is_err()); // too few rows
        assert!(Lut::parse_cube("LUT_1D_SIZE 4\n").is_err());
        let mut f = Frame::new(1, 1, [9, 8, 7, 255]);
        apply_effect(&FilterSpec::lut(), &mut f, 0); // empty path
        apply_effect(&FilterSpec::Lut { path: "/no/such.cube".into(), amount: 1.0 }, &mut f, 0);
        assert_eq!(f.pixel(0, 0), [9, 8, 7, 255]);
    }

    #[test]
    fn image_mask_hides_by_luma_alpha_and_inverse() {
        // 2x1 mask: left black, right white (opaque).
        let path = tmp("mask.png");
        image::RgbaImage::from_raw(2, 1, vec![0, 0, 0, 255, 255, 255, 255, 255]).unwrap().save(&path).unwrap();
        let p: String = path.to_string_lossy().into();
        let mut f = Frame::new(4, 1, [10, 20, 30, 255]); // stretched: left 2 px black, right 2 px white
        apply_effect(&FilterSpec::Mask { path: p.clone(), mode: MaskMode::Luma }, &mut f, 0);
        assert_eq!((0..4).map(|x| f.pixel(x, 0)[3]).collect::<Vec<_>>(), vec![0, 0, 255, 255]);
        let mut inv = Frame::new(4, 1, [10, 20, 30, 255]);
        apply_effect(&FilterSpec::Mask { path: p.clone(), mode: MaskMode::InvertedLuma }, &mut inv, 0);
        assert_eq!((0..4).map(|x| inv.pixel(x, 0)[3]).collect::<Vec<_>>(), vec![255, 255, 0, 0]);
        let mut al = Frame::new(4, 1, [10, 20, 30, 255]);
        apply_effect(&FilterSpec::Mask { path: p, mode: MaskMode::Alpha }, &mut al, 0); // mask is fully opaque
        assert!((0..4).all(|x| al.pixel(x, 0)[3] == 255));
        assert_eq!(f.pixel(0, 0)[..3], [10, 20, 30]); // color untouched, only alpha changes
    }

    #[test]
    fn new_specs_round_trip_through_json() {
        let list = vec![
            Effect::new(FilterSpec::Lut { path: "a.cube".into(), amount: 0.7 }),
            Effect::new(FilterSpec::Mask { path: "m.png".into(), mode: MaskMode::InvertedLuma }),
            Effect::new(FilterSpec::scroll()),
            Effect::new(FilterSpec::flip()),
        ];
        let back: Vec<Effect> = serde_json::from_str(&serde_json::to_string(&list).unwrap()).unwrap();
        assert_eq!(back, list);
    }
}
