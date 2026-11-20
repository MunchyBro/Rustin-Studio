//! Audio inputs with level meters, volume/mute, and per-input WAV recording.
//!
//! Inputs: the default microphone/line-in everywhere, plus "Desktop Audio"
//! (WASAPI loopback of the default output device) on Windows.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex, Weak,
    },
};

/// Minimal 16-bit PCM WAV writer.
struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u32,
}

impl WavWriter {
    fn create(path: &Path, rate: u32, channels: u16) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(b"RIFF")?;
        file.write_all(&0u32.to_le_bytes())?; // patched on finish
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?; // PCM
        file.write_all(&channels.to_le_bytes())?;
        file.write_all(&rate.to_le_bytes())?;
        file.write_all(&(rate * channels as u32 * 2).to_le_bytes())?;
        file.write_all(&(channels * 2).to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&0u32.to_le_bytes())?; // patched on finish
        Ok(Self { file, data_bytes: 0 })
    }

    fn write_samples(&mut self, samples: &[f32]) {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            if self.file.write_all(&v.to_le_bytes()).is_ok() {
                self.data_bytes = self.data_bytes.saturating_add(2);
            }
        }
    }

    fn finish(mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&self.data_bytes.to_le_bytes())?;
        self.file.flush()
    }
}

/// Live feed of an input, resampled to 48 kHz stereo for streaming.
struct Tap {
    buf: VecDeque<f32>,
    phase: f64,
    prev: [f32; 2],
}

impl Tap {
    const RATE: f64 = 48_000.0;
    /// Never hold more than 0.5 s; a stalled consumer must not grow memory.
    const MAX_SAMPLES: usize = 48_000;

    fn new() -> Self { Self { buf: VecDeque::new(), phase: 0.0, prev: [0.0; 2] } }

    fn push(&mut self, samples: &[f32], channels: u16, rate: u32) {
        let ch = channels.max(1) as usize;
        let frames: Vec<[f32; 2]> = samples
            .chunks_exact(ch)
            .map(|f| if ch == 1 { [f[0], f[0]] } else { [f[0], f[1]] })
            .collect();
        let n = frames.len();
        if n == 0 { return; }
        let step = rate as f64 / Self::RATE;
        // Interpolate over [prev] + frames.
        let at = |i: usize| if i == 0 { self.prev } else { frames[i - 1] };
        let mut phase = self.phase;
        while phase < n as f64 {
            let i = phase.floor() as usize;
            let t = (phase - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            self.buf.push_back(a[0] + (b[0] - a[0]) * t);
            self.buf.push_back(a[1] + (b[1] - a[1]) * t);
            phase += step;
        }
        self.phase = phase - n as f64;
        self.prev = frames[n - 1];
        if self.buf.len() > Self::MAX_SAMPLES {
            let drop = self.buf.len() - Self::MAX_SAMPLES / 4;
            self.buf.drain(..drop - drop % 2);
        }
    }
}

struct Shared {
    tap: Mutex<Option<Tap>>,
    /// Feed for local monitoring (speakers), separate from the broadcast tap.
    mon: Mutex<Option<Tap>>,
    monitor: AtomicBool,
    volume: AtomicU32, // f32 bits
    muted: AtomicBool,
    peak: AtomicU32, // f32 bits, reset by the UI each frame
    wav: Mutex<Option<WavWriter>>,
    sample_rate: u32,
    channels: u16,
}

pub struct AudioInput {
    pub name: String,
    /// Linear gain, 1.0 = unity.
    pub volume: f32,
    pub muted: bool,
    /// Also play this input through the speakers (off by default for microphones).
    pub monitor: bool,
    /// Smoothed meter value (linear 0..1+), updated by `AudioMixer::update_levels`.
    pub level: f32,
    shared: Arc<Shared>,
    /// None for virtual inputs (see `AudioFeed`).
    _stream: Option<cpal::Stream>,
    /// For virtual inputs: dies when the producing source is dropped, which removes the input.
    owner: Option<Weak<()>>,
}

fn process(shared: &Shared, samples: &[f32]) {
    let vol = if shared.muted.load(Ordering::Relaxed) { 0.0 } else { f32::from_bits(shared.volume.load(Ordering::Relaxed)) };
    let mut peak = 0f32;
    for s in samples {
        peak = peak.max((s * vol).abs());
    }
    let cur = f32::from_bits(shared.peak.load(Ordering::Relaxed));
    if peak > cur {
        shared.peak.store(peak.to_bits(), Ordering::Relaxed);
    }
    let scaled: Vec<f32> = samples.iter().map(|s| s * vol).collect();
    if let Ok(mut guard) = shared.wav.try_lock() {
        if let Some(w) = guard.as_mut() {
            w.write_samples(&scaled);
        }
    }
    if let Ok(mut guard) = shared.tap.try_lock() {
        if let Some(t) = guard.as_mut() {
            t.push(&scaled, shared.channels, shared.sample_rate);
        }
    }
    if shared.monitor.load(Ordering::Relaxed) {
        if let Ok(mut guard) = shared.mon.try_lock() {
            guard.get_or_insert_with(Tap::new).push(&scaled, shared.channels, shared.sample_rate);
        }
    }
}

fn build_stream<T>(device: &cpal::Device, config: &cpal::StreamConfig, shared: Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let buf: Vec<f32> = data.iter().map(|s| f32::from_sample(*s)).collect();
                process(&shared, &buf);
            },
            |e| eprintln!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

fn open_input(device: &cpal::Device, name: &str, loopback: bool) -> Result<AudioInput, String> {
    let supported = if loopback { device.default_output_config() } else { device.default_input_config() }
        .map_err(|e| e.to_string())?;
    let config: cpal::StreamConfig = supported.clone().into();
    let shared = Arc::new(Shared {
        tap: Mutex::new(None),
        mon: Mutex::new(None),
        monitor: AtomicBool::new(false),
        volume: AtomicU32::new(1.0f32.to_bits()),
        muted: AtomicBool::new(false),
        peak: AtomicU32::new(0),
        wav: Mutex::new(None),
        sample_rate: config.sample_rate.0,
        channels: config.channels,
    });
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(device, &config, shared.clone())?,
        cpal::SampleFormat::I16 => build_stream::<i16>(device, &config, shared.clone())?,
        cpal::SampleFormat::U16 => build_stream::<u16>(device, &config, shared.clone())?,
        other => return Err(format!("unsupported sample format {other:?}")),
    };
    stream.play().map_err(|e| e.to_string())?;
    Ok(AudioInput { name: name.into(), volume: 1.0, muted: false, monitor: false, level: 0.0, shared, _stream: Some(stream), owner: None })
}

/// Input list shared with live taps, so inputs added mid-stream are heard right away.
type SharedInputs = Arc<Mutex<Vec<Arc<Shared>>>>;

/// Pulls mixed 48 kHz stereo f32 audio from all inputs (post-fader, post-mute).
pub struct AudioTap {
    inputs: SharedInputs,
}

/// A virtual audio input fed by code instead of a sound device (e.g. a video file's soundtrack).
/// Samples are interleaved stereo f32 at 48 kHz. It shows up in the mixer with a meter, fader and
/// mute, and is included in recordings and streams.
#[derive(Clone)]
pub struct AudioFeed {
    shared: Arc<Shared>,
}

impl AudioFeed {
    pub fn push(&self, samples: &[f32]) {
        process(&self.shared, samples);
    }
}

impl AudioTap {
    /// Returns exactly `frames * 2` interleaved samples; missing data is silence.
    pub fn pull(&self, frames: usize) -> Vec<f32> {
        let want = frames * 2;
        let mut out = vec![0f32; want];
        for inp in self.inputs.lock().unwrap().iter() {
            if let Some(t) = inp.tap.lock().unwrap().as_mut() {
                let n = want.min(t.buf.len());
                for (o, s) in out.iter_mut().zip(t.buf.drain(..n)) {
                    *o += s;
                }
            }
        }
        for o in &mut out {
            *o = o.clamp(-1.0, 1.0);
        }
        out
    }
}

#[derive(Default)]
pub struct AudioMixer {
    pub inputs: Vec<AudioInput>,
    pub errors: Vec<String>,
    wav_paths: Vec<PathBuf>,
    tap_inputs: SharedInputs,
    tap_active: bool,
    monitor_stream: Option<cpal::Stream>,
}

impl AudioMixer {
    pub fn new() -> Self {
        let mut m = Self::default();
        let host = cpal::default_host();
        if cfg!(windows) {
            match host.default_output_device() {
                Some(d) => match open_input(&d, "Desktop Audio", true) {
                    Ok(i) => m.inputs.push(i),
                    Err(e) => m.errors.push(format!("Desktop Audio: {e}")),
                },
                None => {}
            }
        }
        if let Some(d) = host.default_input_device() {
            match open_input(&d, "Mic/Aux", false) {
                Ok(i) => m.inputs.push(i),
                Err(e) => m.errors.push(format!("Mic/Aux: {e}")),
            }
        }
        *m.tap_inputs.lock().unwrap() = m.inputs.iter().map(|i| i.shared.clone()).collect();
        m
    }

    /// Add a virtual input. The producer must keep the returned token alive for as long as it
    /// is sending audio; when the token is dropped the input disappears from the mixer.
    pub fn add_feed(&mut self, name: &str) -> (AudioFeed, Arc<()>) {
        // With Windows' Desktop Audio loopback running, anything played to the speakers is captured
        // again, so monitoring a feed there would double it in the stream: default to off then.
        let monitor = !self.inputs.iter().any(|i| i.name == "Desktop Audio");
        let shared = Arc::new(Shared {
            tap: Mutex::new(if self.tap_active { Some(Tap::new()) } else { None }),
            mon: Mutex::new(None),
            monitor: AtomicBool::new(monitor),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            wav: Mutex::new(None),
            sample_rate: 48_000,
            channels: 2,
        });
        let token = Arc::new(());
        self.inputs.push(AudioInput {
            name: name.into(),
            volume: 1.0,
            muted: false,
            monitor,
            level: 0.0,
            shared: shared.clone(),
            _stream: None,
            owner: Some(Arc::downgrade(&token)),
        });
        self.tap_inputs.lock().unwrap().push(shared.clone());
        (AudioFeed { shared }, token)
    }

    /// Push the UI's volume/mute values to the audio threads, and refresh meters.
    pub fn update_levels(&mut self) {
        // Drop virtual inputs whose source is gone.
        if self.inputs.iter().any(|i| i.owner.as_ref().map_or(false, |w| w.strong_count() == 0)) {
            self.inputs.retain(|i| i.owner.as_ref().map_or(true, |w| w.strong_count() > 0));
            *self.tap_inputs.lock().unwrap() = self.inputs.iter().map(|i| i.shared.clone()).collect();
        }
        for i in &mut self.inputs {
            i.shared.volume.store(i.volume.to_bits(), Ordering::Relaxed);
            i.shared.muted.store(i.muted, Ordering::Relaxed);
            if i.shared.monitor.swap(i.monitor, Ordering::Relaxed) && !i.monitor {
                *i.shared.mon.lock().unwrap() = None; // stop playing what was buffered
            }
            let peak = f32::from_bits(i.shared.peak.swap(0, Ordering::Relaxed));
            i.level = if peak > i.level { peak } else { i.level * 0.88 };
        }
    }

    /// Start writing each input to `<stem>.<input>.wav`. Returns the files.
    pub fn start_recording(&mut self, stem: &Path) -> Vec<PathBuf> {
        self.wav_paths.clear();
        for i in &self.inputs {
            let tag: String = i.name.chars().filter(|c| c.is_alphanumeric()).collect();
            let path = PathBuf::from(format!("{}.{}.wav", stem.display(), tag));
            match WavWriter::create(&path, i.shared.sample_rate, i.shared.channels) {
                Ok(w) => {
                    *i.shared.wav.lock().unwrap() = Some(w);
                    self.wav_paths.push(path);
                }
                Err(e) => eprintln!("can't create {}: {e}", path.display()),
            }
        }
        self.wav_paths.clone()
    }

    /// Begin feeding a live mix; call `stop_tap` when streaming ends.
    pub fn start_tap(&mut self) -> AudioTap {
        self.tap_active = true;
        for i in &self.inputs {
            *i.shared.tap.lock().unwrap() = Some(Tap::new());
        }
        AudioTap { inputs: self.tap_inputs.clone() }
    }

    pub fn stop_tap(&mut self) {
        self.tap_active = false;
        for i in &self.inputs {
            *i.shared.tap.lock().unwrap() = None;
        }
    }

    pub fn stop_recording(&mut self) {
        for i in &self.inputs {
            if let Some(w) = i.shared.wav.lock().unwrap().take() {
                let _ = w.finish();
            }
        }
    }
}

// ------------------------------------------------------------------ local monitoring

/// Mixes the monitored inputs and converts 48 kHz stereo to the output device's rate and channel
/// count. Pure logic (no device), so it can be tested without speakers.
struct MonitorMixer {
    inputs: SharedInputs,
    src: VecDeque<[f32; 2]>,
    phase: f64,
}

impl MonitorMixer {
    fn new(inputs: SharedInputs) -> Self { Self { inputs, src: VecDeque::new(), phase: 0.0 } }

    /// Pull `frames` 48 kHz stereo frames from every monitored input (silence where none arrived).
    fn pull(&mut self, frames: usize) {
        let mut mix = vec![0f32; frames * 2];
        for inp in self.inputs.lock().unwrap().iter() {
            if !inp.monitor.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(t) = inp.mon.lock().unwrap().as_mut() {
                let n = (frames * 2).min(t.buf.len());
                for (o, s) in mix.iter_mut().zip(t.buf.drain(..n)) {
                    *o += s;
                }
            }
        }
        for f in mix.chunks_exact(2) {
            self.src.push_back([f[0].clamp(-1.0, 1.0), f[1].clamp(-1.0, 1.0)]);
        }
    }

    /// Fill an interleaved output buffer for a device with `channels` channels at `rate` Hz.
    fn fill(&mut self, out: &mut [f32], channels: usize, rate: u32) {
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let step = 48_000.0 / rate.max(1) as f64; // source frames consumed per output frame
        let need = (self.phase + step * frames as f64).ceil() as usize + 2;
        if self.src.len() < need {
            self.pull(need - self.src.len());
        }
        for f in 0..frames {
            let i = self.phase.floor() as usize;
            let t = (self.phase - i as f64) as f32;
            let (a, b) = (self.src[i], self.src[i + 1]);
            let (l, r) = (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t);
            let o = &mut out[f * channels..(f + 1) * channels];
            match channels {
                1 => o[0] = (l + r) * 0.5,
                _ => {
                    o[0] = l;
                    o[1] = r;
                    for c in &mut o[2..] {
                        *c = 0.0;
                    }
                }
            }
            self.phase += step;
        }
        let used = self.phase.floor() as usize;
        self.src.drain(..used.min(self.src.len()));
        self.phase -= used as f64;
    }
}

fn build_output<T>(device: &cpal::Device, config: &cpal::StreamConfig, mut mixer: MonitorMixer) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let (channels, rate) = (config.channels as usize, config.sample_rate.0);
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                scratch.resize(data.len(), 0.0);
                mixer.fill(&mut scratch, channels, rate);
                for (o, s) in data.iter_mut().zip(&scratch) {
                    *o = T::from_sample(*s);
                }
            },
            |e| eprintln!("audio monitor error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

impl AudioMixer {
    pub fn monitor_running(&self) -> bool { self.monitor_stream.is_some() }

    /// Start playing the monitored inputs on the default output device.
    pub fn start_monitor(&mut self) -> Result<(), String> {
        if self.monitor_stream.is_some() {
            return Ok(());
        }
        let device = cpal::default_host().default_output_device().ok_or("no audio output device found")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let config: cpal::StreamConfig = supported.clone().into();
        let mixer = MonitorMixer::new(self.tap_inputs.clone());
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build_output::<f32>(&device, &config, mixer)?,
            cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, mixer)?,
            cpal::SampleFormat::U16 => build_output::<u16>(&device, &config, mixer)?,
            other => return Err(format!("unsupported output format {other:?}")),
        };
        stream.play().map_err(|e| e.to_string())?;
        self.monitor_stream = Some(stream);
        Ok(())
    }

    /// Release the output device (when nothing is being monitored).
    pub fn stop_monitor(&mut self) {
        self.monitor_stream = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_mono_24k_to_stereo_48k() {
        let mut t = Tap::new();
        let input = vec![0.5f32; 2400]; // 0.1 s mono @ 24 kHz
        t.push(&input, 1, 24_000);
        let frames = t.buf.len() / 2;
        assert!((4795..=4805).contains(&frames), "got {frames}");
        // Constant input stays (almost) constant; first frame interpolates from silence.
        assert!(t.buf.iter().skip(8).all(|&v| (v - 0.5).abs() < 1e-4));
    }

    #[test]
    fn feeds_appear_meter_reach_the_tap_and_vanish_with_their_owner() {
        let mut m = AudioMixer::default();
        let tap = m.start_tap();
        let (feed, token) = m.add_feed("clip"); // added while a stream is live
        assert_eq!(m.inputs.len(), 1);
        let tone: Vec<f32> = (0..4800).flat_map(|i| { let s = (i as f32 * 0.1).sin() * 0.5; [s, s] }).collect();
        feed.push(&tone);
        m.update_levels();
        assert!(m.inputs[0].level > 0.3, "meter {}", m.inputs[0].level);
        let pulled = tap.pull(480);
        assert!(pulled.iter().any(|s| s.abs() > 0.1), "tap should carry the feed's audio");
        // Mute silences audio that arrives afterwards (drain what was buffered before it first).
        tap.pull(100_000);
        m.inputs[0].muted = true;
        m.update_levels();
        feed.push(&tone);
        let after = tap.pull(480);
        // (The resampler's one-sample delay lets the previous buffer's last sample through first.)
        assert!(after[2..].iter().all(|s| s.abs() < 1e-6), "muted audio leaked into the tap");
        // Dropping the producer's token removes the input.
        drop(token);
        m.update_levels();
        assert!(m.inputs.is_empty());
        assert!(tap.pull(10).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn buffer_is_bounded() {
        let mut t = Tap::new();
        for _ in 0..100 {
            t.push(&vec![0.1f32; 9600], 2, 48_000);
        }
        assert!(t.buf.len() <= Tap::MAX_SAMPLES);
    }

    fn monitored_mixer() -> (AudioMixer, AudioFeed, Arc<()>, MonitorMixer) {
        let mut m = AudioMixer::default();
        let (feed, token) = m.add_feed("clip");
        assert!(m.inputs[0].monitor, "feeds are monitored by default when there is no Desktop Audio input");
        let mm = MonitorMixer::new(m.tap_inputs.clone());
        (m, feed, token, mm)
    }

    fn tone(frames: usize, amp: f32) -> Vec<f32> {
        (0..frames).flat_map(|i| { let s = (i as f32 * 0.05).sin() * amp; [s, s] }).collect()
    }

    #[test]
    fn monitor_plays_feeds_at_other_rates_and_channel_counts() {
        let (_m, feed, _t, mut mm) = monitored_mixer();
        feed.push(&tone(48_000, 0.5));
        // 44.1 kHz stereo: a 441-frame block consumes 480 source frames.
        let mut out = vec![0f32; 441 * 2];
        mm.fill(&mut out, 2, 44_100);
        assert!(out.iter().any(|s| s.abs() > 0.2));
        // Mono output averages left and right (equal here, so the same level).
        let mut mono = vec![0f32; 480];
        mm.fill(&mut mono, 1, 48_000);
        assert!(mono.iter().any(|s| s.abs() > 0.2));
        // 6-channel output: front pair carries sound, the rest are silent.
        let mut surround = vec![0f32; 6 * 480];
        mm.fill(&mut surround, 6, 48_000);
        assert!(surround.chunks_exact(6).any(|f| f[0].abs() > 0.2 && f[1].abs() > 0.2));
        assert!(surround.chunks_exact(6).all(|f| f[2..].iter().all(|s| *s == 0.0)));
    }

    #[test]
    fn monitor_is_silent_without_data_when_switched_off_or_muted() {
        let (mut m, feed, _t, mut mm) = monitored_mixer();
        let mut out = vec![1f32; 960];
        mm.fill(&mut out, 2, 48_000); // nothing pushed yet: silence, not garbage
        assert!(out.iter().all(|s| s.abs() < 1e-6));

        // Switch monitoring off: audio keeps going to the broadcast path but not the speakers.
        m.inputs[0].monitor = false;
        m.update_levels();
        feed.push(&tone(4800, 0.5));
        let mut out = vec![0f32; 960];
        mm.fill(&mut out, 2, 48_000);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
        assert!(feed.shared.mon.lock().unwrap().is_none(), "no monitor audio is buffered while off");

        // Muted inputs are silent on the speakers too.
        m.inputs[0].monitor = true;
        m.inputs[0].muted = true;
        m.update_levels();
        feed.push(&tone(4800, 0.5));
        let mut out = vec![0f32; 960];
        mm.fill(&mut out, 2, 48_000);
        mm.fill(&mut out, 2, 48_000);
        assert!(out[4..].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn monitoring_does_not_disturb_the_broadcast_tap_and_defaults_avoid_double_audio() {
        let mut m = AudioMixer::default();
        let tap = m.start_tap();
        let (feed, _t) = m.add_feed("clip");
        feed.push(&tone(4800, 0.5));
        assert!(tap.pull(480).iter().any(|s| s.abs() > 0.1), "broadcast gets the audio");
        assert!(feed.shared.mon.lock().unwrap().is_some(), "and so does the monitor buffer");
        // With a Desktop Audio input present (Windows loopback), feeds default to NOT monitored.
        let mut w = AudioMixer::default();
        let (_f, _tk) = w.add_feed("Desktop Audio"); // stand-in with the loopback input's name
        let (_g, _tk2) = w.add_feed("movie");
        assert!(w.inputs[0].monitor);          // the stand-in itself was added first
        assert!(!w.inputs[1].monitor, "double-audio guard");
    }

    #[test]
    fn starting_the_monitor_without_a_device_fails_cleanly() {
        let mut m = AudioMixer::default();
        // Either there is an output device (fine) or we get an error; it must never panic.
        let _ = m.start_monitor();
        m.stop_monitor();
        assert!(!m.monitor_running());
    }
}
