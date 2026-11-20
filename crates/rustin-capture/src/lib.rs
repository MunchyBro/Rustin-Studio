//! Screen, window and image sources.
//!
//! Capture runs on a background thread per source; `render()` just returns the
//! most recent frame, so a slow grab never stalls the compositor.

use rustin_core::{Frame, Source, SourceSpec};
use std::{
    io::Read,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app: String,
    pub width: u32,
    pub height: u32,
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    xcap::Monitor::all()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            Some(MonitorInfo {
                id: m.id().ok()?,
                name: m.name().unwrap_or_else(|_| "Display".into()),
                width: m.width().ok()?,
                height: m.height().ok()?,
                primary: m.is_primary().unwrap_or(false),
            })
        })
        .collect()
}

pub fn list_windows() -> Vec<WindowInfo> {
    xcap::Window::all()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            if w.is_minimized().unwrap_or(false) {
                return None;
            }
            let title = w.title().ok()?;
            let (width, height) = (w.width().ok()?, w.height().ok()?);
            if title.trim().is_empty() || width < 32 || height < 32 {
                return None;
            }
            Some(WindowInfo { id: w.id().ok()?, title, app: w.app_name().unwrap_or_default(), width, height })
        })
        .collect()
}

/// Runs an ffmpeg-family tool without flashing a console window on Windows.
#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

fn tool(name: &str) -> Command {
    let mut cmd = Command::new(name);
    hide_window(&mut cmd);
    cmd
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// True if the file has an audio stream (ffprobe, else ffmpeg's stream listing).
pub fn probe_has_audio(path: &str) -> bool {
    if let Ok(out) = tool("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).contains("audio");
        }
    }
    tool("ffmpeg")
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdout(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).lines().any(|l| l.contains("Audio:")))
        .unwrap_or(false)
}

/// Video size of the first stream: ffprobe if present, else parsed from `ffmpeg -i`.
pub fn probe_size(path: &str) -> Result<(u32, u32), String> {
    if !Path::new(path).exists() {
        return Err(format!("file not found: {path}"));
    }
    if let Ok(out) = tool("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut it = text.trim().split(',').filter_map(|v| v.trim().parse::<u32>().ok());
        if let (Some(w), Some(h)) = (it.next(), it.next()) {
            if w > 0 && h > 0 {
                return Ok((w, h));
            }
        }
    }
    let out = tool("ffmpeg").args(["-hide_banner", "-i"]).arg(path).stdout(Stdio::null()).output().map_err(|_| "ffmpeg isn't installed (needed for video files)".to_string())?;
    let text = String::from_utf8_lossy(&out.stderr);
    for line in text.lines().filter(|l| l.contains("Video:")) {
        for tok in line.split(|c: char| !(c.is_ascii_digit() || c == 'x')) {
            if let Some((a, b)) = tok.split_once('x') {
                if let (Ok(w), Ok(h)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    if w >= 16 && h >= 16 {
                        return Ok((w, h));
                    }
                }
            }
        }
    }
    Err("no video stream found in that file".into())
}

struct Shared {
    frame: Mutex<Option<Frame>>,
    stop: AtomicBool,
    w: AtomicU32,
    h: AtomicU32,
    error: Mutex<Option<String>>,
}

type Grab = Box<dyn FnMut() -> Result<(u32, u32, Vec<u8>), String>>;

fn spawn_worker(
    shared: Arc<Shared>,
    fps: u32,
    make: impl FnOnce() -> Result<Grab, String> + Send + 'static,
) {
    thread::spawn(move || {
        let interval = Duration::from_millis(1000 / fps.max(1) as u64);
        let mut grab = match make() {
            Ok(g) => g,
            Err(e) => {
                *shared.error.lock().unwrap() = Some(e);
                return;
            }
        };
        while !shared.stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match grab() {
                Ok((w, h, data)) => {
                    shared.w.store(w, Ordering::Relaxed);
                    shared.h.store(h, Ordering::Relaxed);
                    *shared.frame.lock().unwrap() = Some(Frame { width: w, height: h, pts_ns: 0, data });
                    *shared.error.lock().unwrap() = None;
                }
                Err(e) => {
                    *shared.error.lock().unwrap() = Some(e);
                    thread::sleep(Duration::from_millis(500));
                }
            }
            if let Some(rest) = interval.checked_sub(t0.elapsed()) {
                thread::sleep(rest);
            }
        }
    });
}

/// Live display or window capture.
/// Receives interleaved stereo f32 samples at 48 kHz (e.g. `AudioFeed::push` in rustin-audio).
pub type AudioSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

pub struct CaptureSource {
    spec: SourceSpec,
    kind: &'static str,
    shared: Arc<Shared>,
    /// Whether this source is currently part of the broadcast; audio is dropped while false.
    live: Arc<AtomicBool>,
}

impl CaptureSource {
    fn new(spec: SourceSpec, kind: &'static str, hint: (u32, u32)) -> (Self, Arc<Shared>) {
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            stop: AtomicBool::new(false),
            w: AtomicU32::new(hint.0),
            h: AtomicU32::new(hint.1),
            error: Mutex::new(None),
        });
        (Self { spec, kind, shared: shared.clone(), live: Arc::new(AtomicBool::new(true)) }, shared)
    }

    /// Capture a whole display. The monitor is looked up by id inside the worker thread.
    pub fn display(monitor: &MonitorInfo, name: &str, fps: u32) -> Self {
        let spec = SourceSpec::Display {
            name: name.into(),
            monitor_name: monitor.name.clone(),
            width: monitor.width,
            height: monitor.height,
        };
        let (src, shared) = Self::new(spec, "Display Capture", (monitor.width, monitor.height));
        let monitor_id = monitor.id;
        spawn_worker(shared, fps, move || {
            let mon = xcap::Monitor::all()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|m| m.id().ok() == Some(monitor_id))
                .ok_or("display not found")?;
            Ok(Box::new(move || {
                let img = mon.capture_image().map_err(|e| e.to_string())?;
                Ok((img.width(), img.height(), img.into_raw()))
            }) as Grab)
        });
        src
    }

    /// Restore a display capture from saved info: match by name, else primary, else first.
    pub fn display_by_name(name: &str, monitor_name: &str, size: (u32, u32), fps: u32) -> Self {
        let monitors = list_monitors();
        let pick = monitors
            .iter()
            .find(|m| m.name == monitor_name)
            .or_else(|| monitors.iter().find(|m| m.primary))
            .or_else(|| monitors.first())
            .cloned()
            .unwrap_or(MonitorInfo { id: u32::MAX, name: monitor_name.into(), width: size.0, height: size.1, primary: false });
        Self::display(&pick, name, fps)
    }

    pub fn window(info: &WindowInfo, name: &str, fps: u32) -> Self {
        Self::window_matching(name, &info.app, &info.title, (info.width, info.height), fps)
    }

    /// Capture a window found by title (or, failing that, by application). Window ids
    /// change between runs, so the worker keeps looking until it finds a match.
    pub fn window_matching(name: &str, app: &str, title: &str, size_hint: (u32, u32), fps: u32) -> Self {
        let spec = SourceSpec::Window {
            name: name.into(),
            app: app.into(),
            title: title.into(),
            width: size_hint.0,
            height: size_hint.1,
        };
        let (src, shared) = Self::new(spec, "Window Capture", size_hint);
        let (app, title) = (app.to_string(), title.to_string());
        spawn_worker(shared, fps, move || {
            let find = move || {
                let mut by_app = None;
                for w in xcap::Window::all().ok()? {
                    if w.title().unwrap_or_default() == title {
                        return Some(w);
                    }
                    if by_app.is_none() && !app.is_empty() && w.app_name().unwrap_or_default() == app {
                        by_app = Some(w);
                    }
                }
                by_app
            };
            let mut win: Option<xcap::Window> = None;
            Ok(Box::new(move || {
                if win.is_none() {
                    win = find();
                }
                match &win {
                    None => Err("waiting for window".to_string()),
                    Some(w) => match w.capture_image() {
                        Ok(img) => Ok((img.width(), img.height(), img.into_raw())),
                        Err(e) => {
                            win = None;
                            Err(e.to_string())
                        }
                    },
                }
            }) as Grab)
        });
        src
    }

    /// Play a video file (looping or once) at up to 1920x1080, silently. Decoding is done by
    /// ffmpeg in real time on a background thread. Fails if the file can't be probed.
    pub fn media(name: &str, path: &str, looping: bool, audio: Option<AudioSink>) -> Result<Self, String> {
        let (w0, h0) = probe_size(path)?;
        // Fit inside 1920x1080 (never upscale), keeping the aspect ratio and even dimensions.
        let k = (1920.0 / w0 as f32).min(1080.0 / h0 as f32).min(1.0);
        let even = |v: f32| (((v.round() as u32).max(2)) / 2) * 2;
        let (w, h) = (even(w0 as f32 * k), even(h0 as f32 * k));
        let spec = SourceSpec::Media { name: name.into(), path: path.into(), looping, width: w, height: h };
        let (src, shared) = Self::new(spec, "Media", (w, h));
        let path = path.to_string();

        // Audio comes back over a loopback socket: we listen, ffmpeg connects and sends f32le PCM.
        let mut audio_port = None;
        if let Some(sink) = audio.filter(|_| probe_has_audio(&path)) {
            if let Ok(listener) = TcpListener::bind("127.0.0.1:0") {
                audio_port = listener.local_addr().ok().map(|a| a.port());
                let (stop, live) = (src.shared.clone(), src.live.clone());
                std::thread::spawn(move || {
                    let _ = listener.set_nonblocking(true);
                    let t0 = Instant::now();
                    let mut sock = loop {
                        if stop.stop.load(Ordering::Relaxed) || t0.elapsed() > Duration::from_secs(15) {
                            return;
                        }
                        match listener.accept() {
                            Ok((s, _)) => break s,
                            Err(_) => std::thread::sleep(Duration::from_millis(20)),
                        }
                    };
                    let _ = sock.set_nonblocking(false);
                    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
                    let mut buf = [0u8; 16384];
                    let mut carry: Vec<u8> = Vec::new();
                    while !stop.stop.load(Ordering::Relaxed) {
                        match sock.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                carry.extend_from_slice(&buf[..n]);
                                let usable = carry.len() / 4 * 4;
                                let samples: Vec<f32> = carry[..usable]
                                    .chunks_exact(4)
                                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                    .collect();
                                carry.drain(..usable);
                                // Keep reading when not live so ffmpeg never stalls; just don't broadcast it.
                                if live.load(Ordering::Relaxed) {
                                    sink(&samples);
                                }
                            }
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                            Err(_) => break,
                        }
                    }
                });
            }
        }

        spawn_worker(shared, 240, move || {
            let mut cmd = tool("ffmpeg");
            cmd.args(["-loglevel", "error"]);
            if looping {
                cmd.args(["-stream_loop", "-1"]);
            }
            cmd.args(["-re", "-i", &path, "-map", "0:v:0", "-an", "-vf"])
                .arg(format!("scale={w}:{h}:flags=bilinear,format=rgba"))
                .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
            if let Some(port) = audio_port {
                cmd.args(["-map", "0:a:0", "-vn", "-f", "f32le", "-ar", "48000", "-ac", "2"])
                    .arg(format!("tcp://127.0.0.1:{port}"));
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = cmd.spawn().map_err(|e| format!("can't start ffmpeg: {e}"))?;
            let mut out = child.stdout.take().ok_or("no ffmpeg output")?;
            let guard = KillOnDrop(child);
            Ok(Box::new(move || {
                let _keep_alive = &guard;
                let mut buf = vec![0u8; (w * h * 4) as usize];
                out.read_exact(&mut buf).map_err(|_| "end of media".to_string())?;
                Ok((w, h, buf))
            }) as Grab)
        });
        Ok(src)
    }

    /// Last capture error, if the source is currently failing.
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Source for CaptureSource {
    fn name(&self) -> &str { self.spec.name() }
    fn set_name(&mut self, name: &str) { self.spec = self.spec.clone().with_name(name); }
    fn kind(&self) -> &'static str { self.kind }
    fn spec(&self) -> SourceSpec { self.spec.clone() }
    fn set_live(&mut self, live: bool) { self.live.store(live, Ordering::Relaxed); }
    fn size(&self) -> (u32, u32) {
        (self.shared.w.load(Ordering::Relaxed).max(1), self.shared.h.load(Ordering::Relaxed).max(1))
    }
    fn render(&mut self, t_ns: u64) -> Frame {
        match &*self.shared.frame.lock().unwrap() {
            Some(f) => {
                let mut f = f.clone();
                f.pts_ns = t_ns;
                f
            }
            None => {
                let (w, h) = self.size();
                Frame::new(w, h, [24, 24, 28, 255])
            }
        }
    }
}

/// A still image (PNG/JPEG) loaded once from disk.
pub struct ImageSource {
    name: String,
    path: String,
    frame: Frame,
}

impl ImageSource {
    pub fn open(name: &str, path: impl AsRef<Path>) -> Result<Self, String> {
        let img = image::open(path.as_ref()).map_err(|e| e.to_string())?.to_rgba8();
        let (width, height) = (img.width(), img.height());
        Ok(Self { name: name.into(), path: path.as_ref().to_string_lossy().into_owned(), frame: Frame { width, height, pts_ns: 0, data: img.into_raw() } })
    }
}

impl Source for ImageSource {
    fn name(&self) -> &str { &self.name }
    fn set_name(&mut self, name: &str) { self.name = name.into(); }
    fn kind(&self) -> &'static str { "Image" }
    fn spec(&self) -> SourceSpec { SourceSpec::Image { name: self.name.clone(), path: self.path.clone() } }
    fn size(&self) -> (u32, u32) { (self.frame.width, self.frame.height) }
    fn render(&mut self, t_ns: u64) -> Frame {
        let mut f = self.frame.clone();
        f.pts_ns = t_ns;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn make_clip(path: &Path) -> bool {
        tool("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=s=64x36:r=10:d=1.5", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(path)
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn media_source_decodes_animates_and_loops() {
        let dir = std::env::temp_dir().join(format!("rustin-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("c.mp4");
        if !make_clip(&clip) { eprintln!("ffmpeg missing: skipped"); return; }
        let path = clip.to_str().unwrap();
        assert_eq!(probe_size(path).unwrap(), (64, 36));

        let mut m = CaptureSource::media("clip", path, true, None).unwrap();
        assert_eq!(m.kind(), "Media");
        assert_eq!(m.size(), (64, 36));
        assert!(matches!(m.spec(), SourceSpec::Media { looping: true, width: 64, height: 36, .. }));

        // Wait for real frames, then check the picture changes over time (testsrc2 animates).
        let t0 = Instant::now();
        let mut first = None;
        while t0.elapsed() < Duration::from_secs(5) {
            let f = m.render(0);
            if f.pixel(0, 0) != [24, 24, 28, 255] { first = Some(f); break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        let first = first.expect("first video frame");
        assert_eq!((first.width, first.height), (64, 36));
        std::thread::sleep(Duration::from_millis(700));
        assert_ne!(m.render(0).data, first.data, "video should animate");
        // Looping: still producing frames well past the 1.5 s clip length.
        std::thread::sleep(Duration::from_millis(1500));
        let a = m.render(0);
        std::thread::sleep(Duration::from_millis(400));
        assert_ne!(m.render(0).data, a.data, "should keep playing after the loop point");
        drop(m);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn media_rejects_missing_and_non_video_files() {
        assert!(CaptureSource::media("x", "/no/such/file.mp4", true, None).is_err());
        let f = std::env::temp_dir().join(format!("rustin-notvideo-{}.mp4", std::process::id()));
        std::fs::write(&f, b"this is not a video").unwrap();
        assert!(CaptureSource::media("x", f.to_str().unwrap(), false, None).is_err());
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn media_audio_reaches_the_sink_only_while_live() {
        let dir = std::env::temp_dir().join(format!("rustin-media-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("av.mp4");
        let ok = tool("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=s=64x36:r=10:d=3"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=3"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&clip)
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok { eprintln!("ffmpeg missing: skipped"); return; }
        let path = clip.to_str().unwrap();
        assert!(probe_has_audio(path));

        let got = Arc::new(Mutex::new(Vec::<f32>::new()));
        let g = got.clone();
        let sink: AudioSink = Arc::new(move |s: &[f32]| g.lock().unwrap().extend_from_slice(s));
        let mut m = CaptureSource::media("av", path, true, Some(sink)).unwrap();
        std::thread::sleep(Duration::from_millis(2200));
        let n1 = got.lock().unwrap().len();
        let peak = got.lock().unwrap().iter().fold(0f32, |a, s| a.max(s.abs()));
        // ~2 s of 48 kHz stereo is ~190k samples; real-time pacing means well under 3 s worth.
        assert!(n1 > 60_000 && n1 < 290_000, "samples {n1}");
        // ffmpeg's sine generator has amplitude 0.125; AAC round-tripping keeps it close to that.
        assert!(peak > 0.05 && peak < 0.2, "expected the quiet 440 Hz tone, peak {peak}");

        m.set_live(false);
        std::thread::sleep(Duration::from_millis(300)); // let any in-flight chunk land
        let frozen = got.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(got.lock().unwrap().len(), frozen, "audio must not be broadcast while not live");
        m.set_live(true);
        std::thread::sleep(Duration::from_millis(700));
        assert!(got.lock().unwrap().len() > frozen, "audio resumes when live again");
        drop(m);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn video_without_audio_never_calls_the_sink() {
        let dir = std::env::temp_dir().join(format!("rustin-media-noaudio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("v.mp4");
        if !make_clip(&clip) { return; }
        assert!(!probe_has_audio(clip.to_str().unwrap()));
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        let sink: AudioSink = Arc::new(move |_| c.store(true, Ordering::Relaxed));
        let m = CaptureSource::media("v", clip.to_str().unwrap(), true, Some(sink)).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        assert!(!called.load(Ordering::Relaxed));
        drop(m);
        std::fs::remove_dir_all(&dir).ok();
    }
}
