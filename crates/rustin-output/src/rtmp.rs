//! RTMP / SRT streaming via ffmpeg: video arrives on stdin, audio over a local TCP socket.

use crate::{encoder::VideoEncoder, ffmpeg_cmd, Output};
use rustin_core::Frame;
use std::{
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Supplies `frames` stereo 48 kHz f32 frames (interleaved) each call.
pub type AudioSource = Box<dyn FnMut(usize) -> Vec<f32> + Send>;

#[derive(Clone, Debug)]
pub struct StreamStats {
    pub alive: bool,
    pub dropped_frames: u64,
    pub last_error: String,
}

/// ffmpeg container for a target URL: FLV for RTMP(S), MPEG-TS for SRT/UDP.
pub fn container_for(url: &str) -> &'static str {
    let u = url.trim().to_ascii_lowercase();
    if u.starts_with("srt://") || u.starts_with("udp://") { "mpegts" } else { "flv" }
}

pub struct RtmpOutput {
    url: String,
    bitrate_kbps: u32,
    audio: Option<AudioSource>,
    encoder: VideoEncoder,
    child: Option<Child>,
    tx: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    audio_thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    flowing: Arc<AtomicBool>,
    last_error: Arc<Mutex<String>>,
}

impl RtmpOutput {
    /// `url` is the full target, e.g. `rtmp://live.twitch.tv/app/<key>` or `srt://host:port?streamid=...`.
    pub fn new(url: impl Into<String>, bitrate_kbps: u32) -> Self {
        Self {
            url: url.into(),
            bitrate_kbps: bitrate_kbps.clamp(300, 50_000),
            audio: None,
            encoder: VideoEncoder::X264,
            child: None,
            tx: None,
            writer: None,
            audio_thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            flowing: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(String::new())),
        }
    }

    pub fn with_encoder(mut self, encoder: VideoEncoder) -> Self {
        self.encoder = encoder;
        self
    }

    pub fn with_audio(mut self, source: AudioSource) -> Self {
        self.audio = Some(source);
        self
    }

    pub fn stats(&self) -> StreamStats {
        StreamStats {
            alive: self.alive.load(Ordering::Relaxed),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }
}

impl Output for RtmpOutput {
    fn start(&mut self, width: u32, height: u32, fps: u32) -> io::Result<()> {
        let kbps = self.bitrate_kbps;
        let mut cmd = ffmpeg_cmd();
        cmd.args(["-y", "-loglevel", "warning", "-nostats"]);

        // Audio input first: ffmpeg listens immediately, so we can connect without
        // waiting for video (it only probes stdin after the audio input is open).
        let mut audio_port = None;
        if self.audio.is_some() {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            drop(listener);
            audio_port = Some(port);
            cmd.args(["-thread_queue_size", "512", "-f", "f32le", "-ar", "48000", "-ac", "2", "-i"])
                .arg(format!("tcp://127.0.0.1:{port}?listen=1"));
        }
        // Frames are stamped with their arrival time and then rate-converted (constant frame rate),
        // so video stays in real time and in sync with audio even if rendering runs late.
        cmd.args(["-use_wallclock_as_timestamps", "1", "-thread_queue_size", "512", "-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-s", &format!("{width}x{height}"), "-i", "-"]);
        if audio_port.is_some() {
            cmd.args(["-map", "1:v", "-map", "0:a", "-c:a", "aac", "-b:a", "160k", "-ar", "48000"]);
        } else {
            cmd.arg("-an");
        }
        cmd.args(["-r", &fps.to_string()])
            .args(crate::cfr_args())
            .args(self.encoder.stream_args(kbps, fps))
            .args(["-f", container_for(&self.url)])
            .arg(&self.url)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;

        self.stop.store(false, Ordering::Relaxed);
        self.alive.store(true, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.last_error.lock().unwrap().clear();

        // Collect ffmpeg's diagnostics; EOF means it exited.
        if let Some(stderr) = child.stderr.take() {
            let (last, alive) = (self.last_error.clone(), self.alive.clone());
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        *last.lock().unwrap() = line;
                    }
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        // Video writer: a slow network can block here without touching the UI thread.
        let mut stdin = child.stdin.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdin"))?;
        // Buffer ~64 MB of frames (3..=30) to ride out ffmpeg's startup and network hiccups.
        let depth = (64 * 1024 * 1024 / (width as usize * height as usize * 4).max(1)).clamp(3, 30);
        let (tx, rx) = sync_channel::<Vec<u8>>(depth);
        let (alive, flowing) = (self.alive.clone(), self.flowing.clone());
        self.flowing.store(false, Ordering::Relaxed);
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

        // Audio feeder: real-time paced 10 ms chunks (silence if the source has nothing).
        if let (Some(port), Some(mut source)) = (audio_port, self.audio.take()) {
            let (stop, alive) = (self.stop.clone(), self.alive.clone());
            self.audio_thread = Some(thread::spawn(move || {
                let mut sock = None;
                for _ in 0..100 {
                    if stop.load(Ordering::Relaxed) { return; }
                    if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) { sock = Some(s); break; }
                    thread::sleep(Duration::from_millis(50));
                }
                let Some(mut sock) = sock else { alive.store(false, Ordering::Relaxed); return };
                let _ = sock.set_nodelay(true);
                let chunk = 480; // 10 ms @ 48 kHz
                let tick = Duration::from_millis(10);
                let mut next = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let samples = source(chunk);
                    let mut bytes = Vec::with_capacity(samples.len() * 4);
                    for s in samples { bytes.extend_from_slice(&s.to_le_bytes()); }
                    if sock.write_all(&bytes).is_err() { break; }
                    next += tick;
                    let now = Instant::now();
                    if next > now { thread::sleep(next - now); } else if now - next > Duration::from_millis(200) { next = now; }
                }
            }));
        }

        self.child = Some(child);
        Ok(())
    }

    fn push_video(&mut self, frame: &Frame) -> io::Result<()> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream ended"));
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
                Err(TrySendError::Disconnected(_)) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream ended")),
            },
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "stream not started")),
        }
    }

    fn uses_wallclock(&self) -> bool { true }

    fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.tx = None; // closes the channel; writer drops stdin
        if let Some(w) = self.writer.take() { let _ = w.join(); }
        if let Some(a) = self.audio_thread.take() { let _ = a.join(); }
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match child.try_wait()? {
                    Some(_) => break,
                    None if Instant::now() > deadline => { let _ = child.kill(); let _ = child.wait(); break; }
                    None => thread::sleep(Duration::from_millis(50)),
                }
            }
        }
        self.alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for RtmpOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_container_from_scheme() {
        assert_eq!(container_for("rtmp://live.twitch.tv/app/key"), "flv");
        assert_eq!(container_for("rtmps://a.rtmps.youtube.com/live2/key"), "flv");
        assert_eq!(container_for("srt://host:9000?mode=caller"), "mpegts");
        assert_eq!(container_for("  SRT://HOST:1  "), "mpegts");
    }
}
