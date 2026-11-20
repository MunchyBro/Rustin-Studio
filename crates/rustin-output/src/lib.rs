//! Outputs: anything that consumes composited frames.

pub mod encoder;
pub mod rtmp;
pub mod stinger;
pub use stinger::StingerStream;
pub use encoder::{detect_encoders, encoder_works, VideoEncoder};
pub use rtmp::{container_for, RtmpOutput, StreamStats};

use rustin_core::Frame;
use std::{
    fs::File,
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub trait Output {
    fn start(&mut self, width: u32, height: u32, fps: u32) -> io::Result<()>;
    fn push_video(&mut self, frame: &Frame) -> io::Result<()>;
    fn stop(&mut self) -> io::Result<()>;
    /// True if the output timestamps frames itself (by arrival time), so callers should push each
    /// newly rendered frame once instead of repeating frames to keep a fixed pace.
    fn uses_wallclock(&self) -> bool { false }
}

/// Writes YUV4MPEG2 (.y4m), playable by ffmpeg, VLC, mpv.
/// Encode afterwards, e.g.: `ffmpeg -i out.y4m -c:v libx264 out.mp4`
pub struct Y4mOutput<W: Write> {
    w: W,
}

impl Y4mOutput<BufWriter<File>> {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self { w: BufWriter::new(File::create(path)?) })
    }
}

impl<W: Write> Y4mOutput<W> {
    pub fn from_writer(w: W) -> Self { Self { w } }
    pub fn into_inner(self) -> W { self.w }
}

impl<W: Write> Output for Y4mOutput<W> {
    fn start(&mut self, width: u32, height: u32, fps: u32) -> io::Result<()> {
        writeln!(self.w, "YUV4MPEG2 W{width} H{height} F{fps}:1 Ip A1:1 C420jpeg")
    }

    fn push_video(&mut self, f: &Frame) -> io::Result<()> {
        let (w, h) = (f.width as usize, f.height as usize);
        let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; cw * ch];
        let mut v = vec![0u8; cw * ch];

        for j in 0..h {
            for i in 0..w {
                let p = (j * w + i) * 4;
                let (r, g, b) = (f.data[p] as f32, f.data[p + 1] as f32, f.data[p + 2] as f32);
                y[j * w + i] = (0.299 * r + 0.587 * g + 0.114 * b).round().clamp(0.0, 255.0) as u8;
            }
        }
        // 4:2:0 chroma: average each 2x2 block (full-range BT.601, matches C420jpeg).
        for cj in 0..ch {
            for ci in 0..cw {
                let (mut r, mut g, mut b, mut n) = (0f32, 0f32, 0f32, 0f32);
                for dj in 0..2 {
                    for di in 0..2 {
                        let (i, j) = ((ci * 2 + di).min(w - 1), (cj * 2 + dj).min(h - 1));
                        let p = (j * w + i) * 4;
                        r += f.data[p] as f32;
                        g += f.data[p + 1] as f32;
                        b += f.data[p + 2] as f32;
                        n += 1.0;
                    }
                }
                let (r, g, b) = (r / n, g / n, b / n);
                u[cj * cw + ci] = (128.0 - 0.168736 * r - 0.331264 * g + 0.5 * b).round().clamp(0.0, 255.0) as u8;
                v[cj * cw + ci] = (128.0 + 0.5 * r - 0.418688 * g - 0.081312 * b).round().clamp(0.0, 255.0) as u8;
            }
        }
        self.w.write_all(b"FRAME\n")?;
        self.w.write_all(&y)?;
        self.w.write_all(&u)?;
        self.w.write_all(&v)
    }

    fn stop(&mut self) -> io::Result<()> { self.w.flush() }
}

/// On Windows, keep child processes from flashing a console window.
#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

pub(crate) fn ffmpeg_cmd() -> Command {
    let mut cmd = Command::new("ffmpeg");
    hide_window(&mut cmd);
    cmd
}

/// True if an `ffmpeg` executable can be launched from PATH.
pub fn ffmpeg_available() -> bool {
    ffmpeg_cmd()
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// ffmpeg's `-fps_mode cfr` replaced `-vsync cfr` in 5.1; pick whichever this ffmpeg understands.
pub(crate) fn cfr_args() -> [&'static str; 2] {
    static NEW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let new = *NEW.get_or_init(|| {
        ffmpeg_cmd()
            .args(["-hide_banner", "-h", "full"])
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("-fps_mode"))
            .unwrap_or(true)
    });
    if new { ["-fps_mode", "cfr"] } else { ["-vsync", "cfr"] }
}

/// Records H.264 MP4 by piping raw RGBA frames into `ffmpeg`.
///
/// Frames are stamped with the wall-clock time they arrive and ffmpeg turns that into a constant
/// frame rate (repeating a frame when rendering is late, dropping one when it is early), so the
/// video keeps real time and stays in sync with audio even when the app can't render every frame.
/// Writing happens on a background thread, so a slow disk never freezes the interface.
pub struct FfmpegOutput {
    path: PathBuf,
    encoder: VideoEncoder,
    quality: u32,
    child: Option<Child>,
    tx: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr_text: Arc<Mutex<String>>,
    alive: Arc<AtomicBool>,
    flowing: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

impl FfmpegOutput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            encoder: VideoEncoder::X264,
            quality: 20,
            child: None,
            tx: None,
            writer: None,
            stderr_reader: None,
            stderr_text: Arc::new(Mutex::new(String::new())),
            alive: Arc::new(AtomicBool::new(false)),
            flowing: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Choose the encoder and constant-quality level (lower number = better quality).
    pub fn with_encoder(mut self, encoder: VideoEncoder, quality: u32) -> Self {
        self.encoder = encoder;
        self.quality = quality;
        self
    }

    /// Frames discarded because ffmpeg/the disk couldn't keep up.
    pub fn dropped_frames(&self) -> u64 { self.dropped.load(Ordering::Relaxed) }
}

impl Output for FfmpegOutput {
    fn start(&mut self, width: u32, height: u32, fps: u32) -> io::Result<()> {
        let mut cmd = ffmpeg_cmd();
        cmd.args(["-y", "-loglevel", "error", "-use_wallclock_as_timestamps", "1", "-thread_queue_size", "512"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-s", &format!("{width}x{height}"), "-i", "-"])
            .args(["-r", &fps.to_string()])
            .args(cfr_args())
            .args(self.encoder.record_args(self.quality));
        // Crash-safe files: fragmented MP4 stays playable if the app dies mid-recording (Matroska
        // is inherently safe). The finished recording is remuxed afterwards for broad compatibility.
        if self.path.extension().map_or(false, |e| e.eq_ignore_ascii_case("mp4")) {
            cmd.args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"]);
        }
        let mut child = cmd
            .arg(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdin"))?;
        let mut stderr = child.stderr.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stderr"))?;
        let stderr_text = self.stderr_text.clone();
        if let Ok(mut text) = stderr_text.lock() { text.clear(); }
        self.stderr_reader = Some(thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            let text = String::from_utf8_lossy(&bytes);
            let tail = if text.len() > 8192 { &text[text.len()-8192..] } else { &text };
            if let Ok(mut saved) = stderr_text.lock() { *saved = tail.to_string(); }
        }));
        // Roughly 64 MB of frames may wait for the encoder before new ones are dropped.
        let depth = (64 * 1024 * 1024 / (width as usize * height as usize * 4).max(1)).clamp(3, 30);
        let (tx, rx) = sync_channel::<Vec<u8>>(depth);
        self.alive.store(true, Ordering::Relaxed);
        self.flowing.store(false, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        let (alive, flowing) = (self.alive.clone(), self.flowing.clone());
        self.writer = Some(thread::spawn(move || {
            while let Ok(buf) = rx.recv() {
                if stdin.write_all(&buf).is_err() {
                    alive.store(false, Ordering::Relaxed);
                    break;
                }
                flowing.store(true, Ordering::Relaxed);
            }
        }));
        self.tx = Some(tx);
        self.child = Some(child);
        Ok(())
    }

    fn push_video(&mut self, frame: &Frame) -> io::Result<()> {
        if !self.alive.load(Ordering::Relaxed) {
            if let Some(mut child) = self.child.take() { let _ = child.wait(); }
            if let Some(reader) = self.stderr_reader.take() { let _ = reader.join(); }
            let detail = self.stderr_text.lock().map(|s| s.trim().to_string()).unwrap_or_default();
            let message = if detail.is_empty() {
                "FFmpeg exited unexpectedly; no diagnostic output was provided".to_string()
            } else {
                format!("FFmpeg exited: {detail}")
            };
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, message));
        }
        match self.tx.as_ref() {
            Some(tx) => match tx.try_send(frame.data.clone()) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => {
                    if self.flowing.load(Ordering::Relaxed) {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "ffmpeg stopped")),
            },
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "recording not started")),
        }
    }

    fn uses_wallclock(&self) -> bool { true }

    fn stop(&mut self) -> io::Result<()> {
        self.tx = None; // lets the writer drain what is queued, then close ffmpeg's input
        if let Some(w) = self.writer.take() { let _ = w.join(); }
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match child.try_wait()? {
                    Some(_) => break,
                    None if Instant::now() > deadline => { let _ = child.kill(); let _ = child.wait(); break; }
                    None => thread::sleep(Duration::from_millis(50)),
                }
            }
        }
        if let Some(reader) = self.stderr_reader.take() { let _ = reader.join(); }
        self.alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}

/// Combine a video with one or more WAV tracks into `out` (video is not re-encoded).
pub fn mux_audio(video: &Path, audio: &[PathBuf], out: &Path) -> io::Result<()> {
    let mut cmd = ffmpeg_cmd();
    cmd.args(["-y", "-loglevel", "error", "-i"]).arg(video);
    for a in audio {
        cmd.arg("-i").arg(a);
    }
    cmd.args(["-map", "0:v"]);
    for i in 0..audio.len() {
        cmd.args(["-map", &format!("{}:a", i + 1)]);
    }
    cmd.args(["-c:v", "copy"]);
    if !audio.is_empty() {
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    if out.extension().map_or(false, |e| e.eq_ignore_ascii_case("mp4")) {
        cmd.args(["-movflags", "+faststart"]); // normal, fast-starting MP4 for sharing and editing
    }
    cmd.arg(out);
    let status = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    if status.success() { Ok(()) } else { Err(io::Error::new(io::ErrorKind::Other, "ffmpeg mux failed")) }
}

/// Saves a single frame as a binary PPM (P6) image.
pub fn save_ppm(frame: &Frame, path: impl AsRef<Path>) -> io::Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    write!(w, "P6\n{} {}\n255\n", frame.width, frame.height)?;
    for px in frame.data.chunks_exact(4) {
        w.write_all(&px[..3])?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y4m_frame_size_is_correct() {
        let mut out = Y4mOutput::from_writer(Vec::new());
        out.start(4, 4, 30).unwrap();
        out.push_video(&Frame::new(4, 4, [255, 0, 0, 255])).unwrap();
        let buf = out.into_inner();
        let header = b"YUV4MPEG2 W4 H4 F30:1 Ip A1:1 C420jpeg\n";
        assert!(buf.starts_with(header));
        // "FRAME\n" + 16 Y + 4 U + 4 V
        assert_eq!(buf.len(), header.len() + 6 + 16 + 4 + 4);
    }

    /// (duration in seconds, number of video frames actually present). Frames are counted from the
    /// packets, because fragmented MP4 files don't store a frame count in their header.
    fn ffprobe_video(path: &std::path::Path) -> (f64, u64) {
        let probe = |args: &[&str]| -> String {
            let out = std::process::Command::new("ffprobe").args(["-v", "error"]).args(args).arg(path).output().expect("ffprobe");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let dur = probe(&["-show_entries", "format=duration", "-of", "csv=p=0"]).parse().unwrap_or(0.0);
        let frames = probe(&["-count_packets", "-select_streams", "v:0", "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
            .parse()
            .unwrap_or(0);
        (dur, frames)
    }

    #[test]
    fn recording_keeps_real_time_when_frames_arrive_slowly() {
        if !ffmpeg_available() { eprintln!("ffmpeg missing: skipped"); return; }
        let path = std::env::temp_dir().join(format!("rustin-sync-{}.mp4", std::process::id()));
        let mut out = FfmpegOutput::new(&path);
        out.start(160, 90, 30).unwrap();
        // A render loop that only manages ~8 frames/s (nominal 30 fps), for 3 seconds.
        let t0 = Instant::now();
        let mut pushed = 0;
        while t0.elapsed() < Duration::from_millis(3000) {
            let mut f = Frame::new(160, 90, [0, 0, 0, 255]);
            f.set_pixel(0, 0, [(pushed * 7) as u8, 0, 0, 255]);
            out.push_video(&f).unwrap();
            pushed += 1;
            thread::sleep(Duration::from_millis(125));
        }
        let wall = t0.elapsed().as_secs_f64();
        out.stop().unwrap();
        let (dur, frames) = ffprobe_video(&path);
        std::fs::remove_file(&path).ok();
        assert!(pushed < 30, "test setup: should be slow, pushed {pushed}");
        // The video must cover the same real time (~3 s, i.e. ~90 frames at 30 fps), not 'pushed' frames' worth (~0.8 s).
        assert!((dur - wall).abs() < 0.4, "video lasted {dur:.2}s but {wall:.2}s of real time passed");
        assert!(frames >= 80 && frames <= 100, "expected ~90 constant-rate frames, got {frames}");
    }

    #[test]
    fn recording_handles_a_fast_producer_without_running_ahead() {
        if !ffmpeg_available() { return; }
        let path = std::env::temp_dir().join(format!("rustin-fast-{}.mp4", std::process::id()));
        let mut out = FfmpegOutput::new(&path);
        out.start(160, 90, 30).unwrap();
        // 100 pushes per second for 2 seconds into a 30 fps recording: extras must be dropped.
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(2000) {
            out.push_video(&Frame::new(160, 90, [10, 20, 30, 255])).unwrap();
            thread::sleep(Duration::from_millis(10));
        }
        let wall = t0.elapsed().as_secs_f64();
        out.stop().unwrap();
        let (dur, frames) = ffprobe_video(&path);
        std::fs::remove_file(&path).ok();
        assert!((dur - wall).abs() < 0.4, "video lasted {dur:.2}s for {wall:.2}s");
        assert!(frames >= 50 && frames <= 70, "expected ~60 frames, got {frames}");
    }

    #[test]
    fn mkv_and_fragmented_mp4_both_record_and_the_remux_makes_a_normal_mp4() {
        if !ffmpeg_available() { return; }
        let dir = std::env::temp_dir().join(format!("rustin-containers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for ext in ["mkv", "mp4"] {
            let path = dir.join(format!("rec.{ext}"));
            let mut out = FfmpegOutput::new(&path).with_encoder(VideoEncoder::X264, 28);
            out.start(128, 72, 30).unwrap();
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(1200) {
                out.push_video(&Frame::new(128, 72, [30, 60, 90, 255])).unwrap();
                thread::sleep(Duration::from_millis(33));
            }
            out.stop().unwrap();
            let (dur, frames) = ffprobe_video(&path);
            assert!((dur - 1.2).abs() < 0.4 && frames > 25, "{ext}: {dur:.2}s, {frames} frames");
        }
        // Remuxing the fragmented MP4 (no audio) produces a normal fast-start file with the same frames.
        let (frag, normal) = (dir.join("rec.mp4"), dir.join("final.mp4"));
        mux_audio(&frag, &[], &normal).unwrap();
        let (_, frames_in) = ffprobe_video(&frag);
        let (dur, frames_out) = ffprobe_video(&normal);
        assert_eq!(frames_in, frames_out);
        assert!((dur - 1.2).abs() < 0.4);
        // A normal MP4 has its frame count in the header; the fragmented one does not.
        let header = |p: &std::path::Path| {
            let o = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=nb_frames", "-of", "csv=p=0"])
                .arg(p).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert_eq!(header(&frag), "N/A");
        assert_ne!(header(&normal), "N/A");
        std::fs::remove_dir_all(&dir).ok();
    }
}
