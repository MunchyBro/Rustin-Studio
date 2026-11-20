use crate::frame::Frame;

/// Per-source video effect, applied before compositing.
pub trait Filter: Send {
    fn apply(&mut self, frame: &mut Frame);
}

/// Multiply RGB by a factor (1.0 = unchanged).
pub struct Brightness(pub f32);

impl Filter for Brightness {
    fn apply(&mut self, frame: &mut Frame) {
        // 256-entry lookup table instead of float math per channel.
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = (i as f32 * self.0).clamp(0.0, 255.0) as u8;
        }
        for px in frame.data.chunks_exact_mut(4) {
            px[0] = lut[px[0] as usize];
            px[1] = lut[px[1] as usize];
            px[2] = lut[px[2] as usize];
        }
    }
}

/// Rec.601 luma grayscale.
pub struct Grayscale;

impl Filter for Grayscale {
    fn apply(&mut self, frame: &mut Frame) {
        for px in frame.data.chunks_exact_mut(4) {
            // Rec.601 luma in fixed point: (77 R + 150 G + 29 B) / 256.
            let y = ((77 * px[0] as u32 + 150 * px[1] as u32 + 29 * px[2] as u32) >> 8) as u8;
            px[0] = y;
            px[1] = y;
            px[2] = y;
        }
    }
}

/// Multiply alpha by a factor in 0..=1.
pub struct Opacity(pub f32);

impl Filter for Opacity {
    fn apply(&mut self, frame: &mut Frame) {
        let k = self.0.clamp(0.0, 1.0);
        for px in frame.data.chunks_exact_mut(4) {
            px[3] = (px[3] as f32 * k) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grayscale_matches_luma() {
        let mut f = Frame::new(1, 1, [255, 0, 0, 255]);
        Grayscale.apply(&mut f);
        assert!((f.pixel(0, 0)[0] as i32 - 76).abs() <= 1); // 0.299 * 255
        let mut w = Frame::new(1, 1, [255, 255, 255, 255]);
        Grayscale.apply(&mut w);
        assert_eq!(w.pixel(0, 0), [255, 255, 255, 255]); // white stays white
    }

    #[test]
    fn brightness_scales_and_clamps() {
        let mut f = Frame::new(1, 1, [100, 200, 50, 255]);
        Brightness(1.5).apply(&mut f);
        assert_eq!(f.pixel(0, 0), [150, 255, 75, 255]);
    }
}
