//! Stinger playback: decodes a video file to RGBA frames with ffmpeg (keeps alpha if present).

use crate::ffmpeg_cmd;
use rustin_core::Frame;
use std::{
    io::{self, Read},
    process::{Child, Stdio},
    sync::mpsc::{sync_channel, Receiver, TryRecvError},
    thread,
};

pub struct StingerStream {
    rx: Receiver<Vec<u8>>,
    child: Child,
    w: u32,
    h: u32,
    next_idx: u64,
    current: Option<Frame>,
    done: bool,
    frames: u64,
}

impl StingerStream {
    /// Start decoding `path`, scaled to `w`x`h` at `fps`. Frames are read on demand.
    pub fn open(path: &str, w: u32, h: u32, fps: u32) -> io::Result<Self> {
        let mut child = ffmpeg_cmd()
            .args(["-loglevel", "error", "-i", path, "-an", "-vf"])
            .arg(format!("fps={fps},scale={w}:{h}:flags=bilinear,format=rgba"))
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut out = child.stdout.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdout"))?;
        let (tx, rx) = sync_channel::<Vec<u8>>(6); // small look-ahead; decoding pauses when full
        let size = (w * h * 4) as usize;
        thread::spawn(move || loop {
            let mut buf = vec![0u8; size];
            if out.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                break;
            }
        });
        Ok(Self { rx, child, w, h, next_idx: 0, current: None, done: false, frames: 0 })
    }

    /// The frame to show at playback index `idx` (frame number at the chosen fps). Frames the
    /// clock has already passed are skipped; if decoding lags, the previous frame is held.
    pub fn frame_at(&mut self, idx: u64) -> Option<&Frame> {
        while self.next_idx <= idx && !self.done {
            match self.rx.try_recv() {
                Ok(data) => {
                    self.current = Some(Frame { width: self.w, height: self.h, pts_ns: 0, data });
                    self.next_idx += 1;
                    self.frames += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.done = true,
            }
        }
        self.current.as_ref()
    }

    /// True once the whole clip has been consumed (or it couldn't be decoded).
    pub fn finished(&self) -> bool { self.done }

    /// Number of frames decoded so far (0 + finished means the file couldn't be played).
    pub fn frames_decoded(&self) -> u64 { self.frames }
}

impl Drop for StingerStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg_available;
    use std::time::{Duration, Instant};

    /// 1-second, 10 fps, half-transparent red clip as .mov with alpha (needs ffmpeg).
    fn make_clip(dir: &std::path::Path) -> Option<String> {
        if !ffmpeg_available() { return None; }
        let path = dir.join("stinger.mov");
        let ok = ffmpeg_cmd()
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=red@0.5:s=32x18:r=10:d=1,format=rgba"])
            .args(["-c:v", "qtrle"]).arg(&path)
            .status().map(|s| s.success()).unwrap_or(false);
        ok.then(|| path.to_string_lossy().into_owned())
    }

    #[test]
    fn decodes_frames_with_alpha_and_finishes() {
        let dir = std::env::temp_dir().join(format!("rustin-stinger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(clip) = make_clip(&dir) else { eprintln!("ffmpeg missing: skipped"); return };
        let mut s = StingerStream::open(&clip, 64, 36, 10).unwrap(); // scaled up on decode
        let t0 = Instant::now();
        let mut got = None;
        while t0.elapsed() < Duration::from_secs(5) {
            if let Some(f) = s.frame_at(0) { got = Some(f.clone()); break; }
            thread::sleep(Duration::from_millis(10));
        }
        let f = got.expect("first frame decoded");
        assert_eq!((f.width, f.height), (64, 36));
        let p = f.pixel(10, 10);
        assert!(p[0] > 200 && p[1] < 40 && p[2] < 40, "red expected, got {p:?}");
        assert!((100..160).contains(&p[3]), "half alpha expected, got {}", p[3]);
        // Run to the end of the clip.
        let mut idx = 0;
        while !s.finished() && t0.elapsed() < Duration::from_secs(8) {
            idx += 1;
            s.frame_at(idx);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(s.finished());
        assert!((9..=11).contains(&s.frames_decoded()), "frames {}", s.frames_decoded());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_finishes_with_no_frames() {
        if !ffmpeg_available() { return; }
        let mut s = StingerStream::open("/definitely/not/here.mov", 16, 9, 30).unwrap();
        let t0 = Instant::now();
        while !s.finished() && t0.elapsed() < Duration::from_secs(5) {
            s.frame_at(0);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(s.finished());
        assert_eq!(s.frames_decoded(), 0);
    }
}
