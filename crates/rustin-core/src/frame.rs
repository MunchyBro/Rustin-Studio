/// A video frame in tightly packed RGBA8 (non-premultiplied alpha).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in nanoseconds.
    pub pts_ns: u64,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&rgba);
        }
        Self { width, height, pts_ns: 0, data }
    }

    pub fn transparent(width: u32, height: u32) -> Self {
        Self::new(width, height, [0, 0, 0, 0])
    }

    /// Copy out a sub-rectangle trimmed by `[left, top, right, bottom]` (at least 1x1 remains).
    pub fn cropped(&self, crop: [u32; 4]) -> Frame {
        let x0 = crop[0].min(self.width - 1);
        let y0 = crop[1].min(self.height - 1);
        let x1 = self.width.saturating_sub(crop[2]).max(x0 + 1);
        let y1 = self.height.saturating_sub(crop[3]).max(y0 + 1);
        let (nw, nh) = (x1 - x0, y1 - y0);
        let mut data = Vec::with_capacity((nw * nh * 4) as usize);
        for y in y0..y1 {
            let start = ((y * self.width + x0) * 4) as usize;
            data.extend_from_slice(&self.data[start..start + (nw * 4) as usize]);
        }
        Frame { width: nw, height: nh, pts_ns: self.pts_ns, data }
    }

    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }

    #[inline]
    pub fn set_pixel(&mut self, x: u32, y: u32, p: [u8; 4]) {
        let i = ((y * self.width + x) * 4) as usize;
        self.data[i..i + 4].copy_from_slice(&p);
    }
}
