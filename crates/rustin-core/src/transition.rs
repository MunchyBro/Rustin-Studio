use crate::frame::Frame;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    Cut,
    #[default]
    Fade,
    SlideLeft,
    SlideRight,
    Wipe,
    /// A video clip played over the scene change (see `rustin-output::StingerStream`).
    Stinger,
}

impl TransitionKind {
    pub const ALL: [TransitionKind; 6] = [
        TransitionKind::Cut,
        TransitionKind::Fade,
        TransitionKind::SlideLeft,
        TransitionKind::SlideRight,
        TransitionKind::Wipe,
        TransitionKind::Stinger,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TransitionKind::Cut => "Cut",
            TransitionKind::Fade => "Fade",
            TransitionKind::SlideLeft => "Slide (from right)",
            TransitionKind::SlideRight => "Slide (from left)",
            TransitionKind::Wipe => "Wipe",
            TransitionKind::Stinger => "Stinger (video)",
        }
    }
}

/// Blend `a` (outgoing scene) into `b` (incoming scene) at progress `t` in 0..=1.
pub fn apply(kind: TransitionKind, a: &Frame, b: &Frame, t: f32) -> Frame {
    assert_eq!((a.width, a.height), (b.width, b.height), "frame size mismatch");
    let t = t.clamp(0.0, 1.0);
    let (w, h) = (a.width as usize, a.height as usize);
    // Pixels already covered by the incoming scene.
    let k = ((w as f32 * t).round() as usize).min(w);
    let row = w * 4;
    let mut out = Frame { width: a.width, height: a.height, pts_ns: b.pts_ns, data: Vec::with_capacity(row * h) };
    match kind {
        TransitionKind::Cut => return Frame { pts_ns: b.pts_ns, ..b.clone() },
        // A stinger is composited by the studio; if it can't play, it degrades to a fade.
        TransitionKind::Fade | TransitionKind::Stinger => return crossfade(a, b, t),
        TransitionKind::SlideLeft => {
            // Incoming scene enters from the right, pushing the old one left.
            for y in 0..h {
                out.data.extend_from_slice(&a.data[y * row + k * 4..(y + 1) * row]);
                out.data.extend_from_slice(&b.data[y * row..y * row + k * 4]);
            }
        }
        TransitionKind::SlideRight => {
            for y in 0..h {
                out.data.extend_from_slice(&b.data[y * row + (w - k) * 4..(y + 1) * row]);
                out.data.extend_from_slice(&a.data[y * row..y * row + (w - k) * 4]);
            }
        }
        TransitionKind::Wipe => {
            // Incoming scene is revealed from the left edge.
            for y in 0..h {
                out.data.extend_from_slice(&b.data[y * row..y * row + k * 4]);
                out.data.extend_from_slice(&a.data[y * row + k * 4..(y + 1) * row]);
            }
        }
    }
    out
}

/// Alpha-composite `top` over `base` (same size). Used for stinger video frames.
pub fn overlay(base: &mut Frame, top: &Frame) {
    if (base.width, base.height) != (top.width, top.height) {
        return;
    }
    for (d, s) in base.data.chunks_exact_mut(4).zip(top.data.chunks_exact(4)) {
        let a = s[3] as u32;
        if a == 0 {
            continue;
        }
        if a == 255 {
            d[..3].copy_from_slice(&s[..3]);
        } else {
            let ia = 255 - a;
            for c in 0..3 {
                d[c] = ((s[c] as u32 * a + d[c] as u32 * ia + 127) / 255) as u8;
            }
        }
        d[3] = 255;
    }
}

/// Linear cross-fade between two equally sized frames. `t` in 0..=1.
pub fn crossfade(a: &Frame, b: &Frame, t: f32) -> Frame {
    assert_eq!((a.width, a.height), (b.width, b.height), "frame size mismatch");
    let t = t.clamp(0.0, 1.0);
    let data = a
        .data
        .iter()
        .zip(&b.data)
        .map(|(&x, &y)| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8)
        .collect();
    Frame { width: a.width, height: a.height, pts_ns: b.pts_ns, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x1 frame whose pixels are numbered so movement is visible.
    fn ramp(base: u8) -> Frame {
        let mut f = Frame::new(4, 1, [0, 0, 0, 255]);
        for x in 0..4 {
            f.set_pixel(x, 0, [base + x as u8, 0, 0, 255]);
        }
        f
    }

    fn reds(f: &Frame) -> Vec<u8> { (0..f.width).map(|x| f.pixel(x, 0)[0]).collect() }

    #[test]
    fn overlay_respects_alpha() {
        let mut base = Frame::new(2, 1, [0, 0, 100, 255]);
        let mut top = Frame::transparent(2, 1);
        top.set_pixel(0, 0, [200, 0, 0, 255]); // opaque
        top.set_pixel(1, 0, [200, 0, 0, 128]); // half transparent
        overlay(&mut base, &top);
        assert_eq!(base.pixel(0, 0), [200, 0, 0, 255]);
        let p = base.pixel(1, 0);
        assert!((p[0] as i32 - 100).abs() <= 1 && (p[2] as i32 - 50).abs() <= 1, "{p:?}");
    }

    #[test]
    fn slide_left_pushes_old_scene_out() {
        let (a, b) = (ramp(10), ramp(20));
        assert_eq!(reds(&apply(TransitionKind::SlideLeft, &a, &b, 0.0)), vec![10, 11, 12, 13]);
        assert_eq!(reds(&apply(TransitionKind::SlideLeft, &a, &b, 0.5)), vec![12, 13, 20, 21]);
        assert_eq!(reds(&apply(TransitionKind::SlideLeft, &a, &b, 1.0)), vec![20, 21, 22, 23]);
    }

    #[test]
    fn slide_right_enters_from_left() {
        let (a, b) = (ramp(10), ramp(20));
        assert_eq!(reds(&apply(TransitionKind::SlideRight, &a, &b, 0.5)), vec![22, 23, 10, 11]);
        assert_eq!(reds(&apply(TransitionKind::SlideRight, &a, &b, 1.0)), vec![20, 21, 22, 23]);
    }

    #[test]
    fn wipe_reveals_from_left_and_cut_is_instant() {
        let (a, b) = (ramp(10), ramp(20));
        assert_eq!(reds(&apply(TransitionKind::Wipe, &a, &b, 0.5)), vec![20, 21, 12, 13]);
        assert_eq!(reds(&apply(TransitionKind::Cut, &a, &b, 0.0)), vec![20, 21, 22, 23]);
    }

    #[test]
    fn midpoint_is_average() {
        let a = Frame::new(2, 2, [0, 0, 0, 255]);
        let b = Frame::new(2, 2, [200, 100, 50, 255]);
        let m = crossfade(&a, &b, 0.5);
        assert_eq!(m.pixel(0, 0), [100, 50, 25, 255]);
    }
}
