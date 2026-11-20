// Release builds on Windows are GUI apps: no console window behind the interface.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use rustin_audio::AudioMixer;
use rustin_gpu::GpuCompositor;
use rustin_hotkeys::{combo_to_strings, display_combo, HotkeyAction, HotkeyEvent, HotkeyPoller};
use rustin_capture::{list_monitors, list_windows, probe_has_audio, AudioSink, CaptureSource, ImageSource, MonitorInfo, WindowInfo};
use rustin_core::{
    source::{ColorSource, TestPatternSource},
    Effect, FilterSpec, MaskMode, SceneItem, Source, TextSource, TransitionKind,
};
use rustin_gui::{
    browse::{self, Browser, DropKind, FONT_EXTS, IMAGE_EXTS, VIDEO_EXTS},
    persist::{self, AudioPref, CollectionSpec, Settings},
    stream_target, Studio,
};
use rustin_output::{detect_encoders, ffmpeg_available, mux_audio, FfmpegOutput, Output, RtmpOutput, VideoEncoder, Y4mOutput};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CANVAS_SIZES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (854, 480)];
const FPS_CHOICES: [u32; 3] = [24, 30, 60];
const SERVICES: [(&str, &str); 4] = [
    ("Twitch", "rtmp://live.twitch.tv/app"),
    ("YouTube", "rtmp://a.rtmp.youtube.com/live2"),
    ("Custom RTMP", ""),
    ("SRT", "srt://"),
];

// ---------------------------------------------------------------- theme
fn apply_obs_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    let bg = Color32::from_rgb(0x1b, 0x1c, 0x20);
    let panel = Color32::from_rgb(0x26, 0x27, 0x2c);
    let widget = Color32::from_rgb(0x3a, 0x3c, 0x44);
    v.panel_fill = panel;
    v.window_fill = panel;
    v.extreme_bg_color = bg;
    v.faint_bg_color = Color32::from_rgb(0x2b, 0x2c, 0x32);
    v.widgets.inactive.bg_fill = widget;
    v.widgets.inactive.weak_bg_fill = widget;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x4a, 0x4d, 0x57);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x4a, 0x4d, 0x57);
    v.selection.bg_fill = Color32::from_rgb(0x1f, 0x4f, 0x8f);
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    ctx.set_visuals(v);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(6.0, 4.0);
    style.spacing.button_padding = Vec2::new(8.0, 3.0);
    ctx.set_style(style);
}

// ---------------------------------------------------------------- recording
struct Recorder {
    out: Box<dyn Output>,
    started: Instant,
    frames: u64,
    video_path: PathBuf,
    final_path: PathBuf,
    audio: Vec<PathBuf>,
    mp4: bool,
}

fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // civil-from-days (Howard Hinnant), UTC
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

// ---------------------------------------------------------------- dialogs / drag state
#[derive(Clone, Copy, PartialEq)]
enum NewKind { Display, Window, Image, Media, Color, Text, Pattern }

impl NewKind {
    fn label(self) -> &'static str {
        match self {
            NewKind::Display => "Display Capture",
            NewKind::Window => "Window Capture",
            NewKind::Image => "Image",
            NewKind::Media => "Media (video)",
            NewKind::Color => "Color Source",
            NewKind::Text => "Text",
            NewKind::Pattern => "Test Pattern",
        }
    }
}

struct AddDialog {
    kind: NewKind,
    name: String,
    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    sel: usize,
    path: String,
    color: [u8; 4],
    text: String,
    size: f32,
    looping: bool,
}

/// Where a file chosen in the browser goes.
#[derive(Clone, Copy, PartialEq)]
enum BrowseTarget { AddPath, Stinger, Font, RecDir, EffectPath(usize) }

#[derive(Clone, Copy, PartialEq)]
enum Corner { TL, TR, BL, BR }

enum Drag {
    Move { grab: (f32, f32), start: (i32, i32) },
    Scale { corner: Corner, anchor: (f32, f32), src: (f32, f32) },
}

enum Action {
    SelectScene(usize),
    AddScene,
    RemoveScene(usize),
    Select(usize),
    RemoveItem(usize),
    MoveItem(usize, i32),
    OpenAdd(NewKind),
    Transition,
}

struct Streamer {
    out: RtmpOutput,
    started: Instant,
    frames: u64,
    key: String,
}

// ---------------------------------------------------------------- app
struct RustinApp {
    studio: Studio,
    clock: Instant,
    tex_program: Option<egui::TextureHandle>,
    tex_preview: Option<egui::TextureHandle>,
    recorder: Option<Recorder>,
    streamer: Option<Streamer>,
    settings: Settings,
    persist: bool,
    last_save: Instant,
    saved: (String, String),
    show_settings: bool,
    show_props: bool,
    show_keys: bool,
    add: Option<AddDialog>,
    mixer: AudioMixer,
    hotkeys: HotkeyPoller,
    capturing: Option<HotkeyAction>,
    monitor_failed: bool,
    /// Result of probing which video encoders actually work here (None while still checking).
    encoders: Arc<Mutex<Option<Vec<(VideoEncoder, bool)>>>>,
    browser: Option<(BrowseTarget, Browser)>,
    browse_result: Option<(BrowseTarget, String)>,
    status: String,
    drag: Option<Drag>,
    frame_times: VecDeque<Instant>,
    render_ms: f32,
    ffmpeg: bool,
}

impl RustinApp {
    fn new(demo: bool, display: bool, stream_server: Option<String>) -> Self {
        // Demo mode is ephemeral: it neither loads nor overwrites your saved setup.
        let persist_on = !demo;
        let mut settings: Settings = if persist_on { persist::load_json("settings.json").unwrap_or_default() } else { Settings::default() };
        settings.canvas = settings.canvas.min(CANVAS_SIZES.len() - 1);
        settings.fps = settings.fps.min(FPS_CHOICES.len() - 1);
        settings.service = settings.service.min(SERVICES.len() - 1);
        let (cw, ch) = CANVAS_SIZES[settings.canvas];
        let studio = if demo { Studio::demo(cw, ch) } else { Studio::new(cw, ch) };
        let mut app = Self {
            studio,
            clock: Instant::now(),
            tex_program: None,
            tex_preview: None,
            recorder: None,
            streamer: None,
            settings,
            persist: persist_on,
            last_save: Instant::now(),
            saved: (String::new(), String::new()),
            show_settings: false,
            show_props: false,
            show_keys: false,
            add: None,
            mixer: AudioMixer::new(),
            hotkeys: HotkeyPoller::new(),
            capturing: None,
            monitor_failed: false,
            encoders: Arc::new(Mutex::new(None)),
            browser: None,
            browse_result: None,
            status: String::new(),
            drag: None,
            frame_times: VecDeque::new(),
            render_ms: 0.0,
            ffmpeg: ffmpeg_available(),
        };
        if persist_on {
            if let Some(spec) = persist::load_json::<CollectionSpec>("scenes.json") {
                let warnings = persist::restore(&mut app.studio, &spec, 30, &mut |n| {
                    let (feed, token) = app.mixer.add_feed(n);
                    let sink: AudioSink = Arc::new(move |s: &[f32]| {
                        let _keep = &token;
                        feed.push(s);
                    });
                    Some(sink)
                });
                if !warnings.is_empty() {
                    app.status = format!("Restored scenes; {}", warnings.join("; "));
                }
            }
            for pref in &app.settings.audio {
                if let Some(inp) = app.mixer.inputs.iter_mut().find(|i| i.name == pref.name) {
                    inp.volume = pref.volume;
                    inp.muted = pref.muted;
                    inp.monitor = pref.monitor;
                }
            }
        }
        app.hotkeys.load(&app.settings.hotkeys);
        if app.ffmpeg {
            // Test-encode with each encoder in the background (a second or two).
            let slot = app.encoders.clone();
            std::thread::spawn(move || {
                let found = detect_encoders();
                *slot.lock().unwrap() = Some(found);
            });
        }
        if app.settings.gpu {
            app.apply_compositor();
        }
        if let Some(url) = stream_server {
            app.settings.service = if url.to_ascii_lowercase().starts_with("srt://") { 3 } else { 2 };
            app.settings.server = url;
        }
        if display {
            if let Some(m) = list_monitors().into_iter().find(|m| m.primary).or_else(|| list_monitors().into_iter().next()) {
                let src = CaptureSource::display(&m, "Display Capture", 30);
                app.studio.add_item(SceneItem::new(Box::new(src)));
                let i = app.studio.edit_scene().items.len() - 1;
                app.studio.fit_to_screen(i);
            }
        }
        app
    }

    fn fps(&self) -> u32 { FPS_CHOICES[self.settings.fps] }

    // ---- recording ----
    fn start_recording(&mut self) {
        let dir = PathBuf::from(&self.settings.rec_dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.status = format!("Can't create {}: {e}", dir.display());
            return;
        }
        let stem = dir.join(format!("Rustin-{}", timestamp()));
        let (w, h) = self.studio.size();
        let fps = self.fps();
        let mp4 = self.ffmpeg;
        let ext = if self.settings.rec_format == "mkv" { "mkv" } else { "mp4" };
        let (encoder, encoder_note) = self.chosen_encoder();
        let video_path = if mp4 { stem.with_extension(format!("video.{ext}")) } else { stem.with_extension("y4m") };
        let final_path = if mp4 { stem.with_extension(ext) } else { video_path.clone() };
        let mut out: Box<dyn Output> = if mp4 {
            Box::new(FfmpegOutput::new(&video_path).with_encoder(encoder, self.settings.rec_quality))
        } else {
            match Y4mOutput::create(&video_path) {
                Ok(o) => Box::new(o),
                Err(e) => { self.status = format!("Can't create file: {e}"); return; }
            }
        };
        if let Err(e) = out.start(w, h, fps) {
            self.status = format!("Can't start recording: {e}");
            return;
        }
        let audio = self.mixer.start_recording(&stem);
        self.recorder = Some(Recorder { out, started: Instant::now(), frames: 0, video_path, final_path, audio, mp4 });
        self.status = match encoder_note {
            Some(n) => format!("Recording started ({n})"),
            None => "Recording started".into(),
        };
    }

    fn stop_recording(&mut self) {
        let Some(mut r) = self.recorder.take() else { return };
        let _ = r.out.stop();
        self.mixer.stop_recording();
        if r.mp4 {
            // The live file is crash-safe but unpolished (fragmented MP4 / bare MKV); remux it, adding
            // the audio tracks, into the final file. If that fails, the original is kept.
            let result = mux_audio(&r.video_path, &r.audio, &r.final_path);
            if result.is_ok() {
                let _ = std::fs::remove_file(&r.video_path);
                for a in &r.audio { let _ = std::fs::remove_file(a); }
            }
            self.status = match result {
                Ok(()) => format!("Recording saved: {}", r.final_path.display()),
                Err(e) => format!("Saved video at {} (finishing it failed: {e})", r.video_path.display()),
            };
        } else {
            self.status = format!("Saved {} (install ffmpeg for MP4 output)", r.final_path.display());
        }
    }

    /// The encoder to use: the one chosen in Settings if it has been verified to work, else x264.
    /// The second value explains a fallback, for the status bar.
    fn chosen_encoder(&mut self) -> (VideoEncoder, Option<String>) {
        let want = VideoEncoder::from_id(&self.settings.encoder).unwrap_or(VideoEncoder::X264);
        if want == VideoEncoder::X264 {
            return (want, None);
        }
        let verdict = self.encoders.lock().unwrap().as_ref().map(|l| l.contains(&(want, true)));
        match verdict {
            Some(true) => (want, None),
            Some(false) => (VideoEncoder::X264, Some(format!("{} isn't available here, used x264 (CPU)", want.label()))),
            None => (VideoEncoder::X264, Some(format!("still checking {}, used x264 (CPU) this time", want.label()))),
        }
    }

    /// A mixer input for a media source's audio; it disappears when the source is dropped.
    fn media_sink(&mut self, name: &str) -> AudioSink {
        let (feed, token) = self.mixer.add_feed(name);
        Arc::new(move |samples: &[f32]| {
            let _keep_input_alive = &token;
            feed.push(samples);
        })
    }

    // ---- audio monitoring ----
    /// Open the speakers when any input is being monitored, and release them when none is.
    fn ensure_monitor(&mut self) {
        let want = self.mixer.inputs.iter().any(|i| i.monitor);
        if !want {
            self.monitor_failed = false; // the user can try again by ticking Monitor
            if self.mixer.monitor_running() {
                self.mixer.stop_monitor();
            }
        } else if !self.mixer.monitor_running() && !self.monitor_failed {
            if let Err(e) = self.mixer.start_monitor() {
                self.monitor_failed = true;
                for i in &mut self.mixer.inputs {
                    i.monitor = false;
                }
                self.status = format!("Audio monitoring unavailable: {e}");
            }
        }
    }

    // ---- compositor ----
    /// Switch between CPU and GPU compositing according to the setting; the GPU path falls back
    /// to the CPU (and unticks the setting) if there is no usable adapter.
    fn apply_compositor(&mut self) {
        if !self.settings.gpu {
            self.studio.comp.backend = None;
            self.status = "Compositing on the CPU".into();
            return;
        }
        match GpuCompositor::new() {
            Ok(g) => {
                self.status = if g.is_software() {
                    format!("GPU compositing is running on a software renderer ({}); the CPU path is faster here", g.adapter_name())
                } else {
                    format!("Compositing on {}", g.adapter_name())
                };
                self.studio.comp.backend = Some(Box::new(g));
            }
            Err(e) => {
                self.settings.gpu = false;
                self.studio.comp.backend = None;
                self.status = format!("GPU compositing unavailable ({e}); using the CPU");
            }
        }
    }

    // ---- files ----
    fn open_browser(&mut self, target: BrowseTarget, start: &str, exts: &[&str], folder: bool) {
        self.browser = Some((target, Browser::new(start, exts, folder)));
    }

    /// A file dropped onto the window: images and videos become sources, fonts go to the selected text.
    fn handle_drop(&mut self, p: &Path) {
        let path = p.to_string_lossy().into_owned();
        let name = browse::stem(p);
        let kind = match browse::classify(p) {
            DropKind::Image => NewKind::Image,
            DropKind::Video => NewKind::Media,
            DropKind::Font => {
                let sel = self.studio.selected;
                let text = sel.and_then(|i| self.studio.edit_scene_mut().items.get_mut(i)).and_then(|it| it.source.edit_text());
                self.status = match text {
                    Some(t) => { t.font_path = path; t.update(); format!("Font set to {name}") }
                    None => "Select a text source first, then drop a font file on it".into(),
                };
                return;
            }
            DropKind::Other => {
                self.status = format!("Can't use {name}: drop an image, video or font file");
                return;
            }
        };
        let d = AddDialog { kind, name: name.clone(), monitors: Vec::new(), windows: Vec::new(), sel: 0, path, color: [255; 4], text: String::new(), size: 0.0, looping: true };
        self.status = match self.create_source(&d) {
            Ok(()) => format!("Added {name}"),
            Err(e) => format!("Couldn't add {name}: {e}"),
        };
    }

    // ---- hotkeys ----
    fn run_hotkey(&mut self, a: HotkeyAction, actions: &mut Vec<Action>) {
        let n = self.studio.scenes.scenes.len();
        match a {
            HotkeyAction::ToggleRecord => {
                if self.recorder.is_some() { self.stop_recording(); } else { self.start_recording(); }
            }
            HotkeyAction::ToggleStream => {
                if self.streamer.is_some() { self.stop_streaming(None); } else { self.start_streaming(); }
            }
            HotkeyAction::ToggleStudioMode => {
                let on = !self.studio.studio_mode;
                self.studio.set_studio_mode(on);
            }
            HotkeyAction::Transition => actions.push(Action::Transition),
            HotkeyAction::NextScene => actions.push(Action::SelectScene((self.studio.edit_index() + 1) % n)),
            HotkeyAction::PrevScene => actions.push(Action::SelectScene((self.studio.edit_index() + n - 1) % n)),
        }
    }

    // ---- persistence ----
    /// Write scenes/settings to the config dir when they changed (checked every few seconds).
    fn autosave(&mut self, force: bool) {
        if !self.persist || (!force && self.last_save.elapsed().as_secs() < 3) {
            return;
        }
        self.last_save = Instant::now();
        self.settings.audio = self
            .mixer
            .inputs
            .iter()
            .map(|i| AudioPref { name: i.name.clone(), volume: i.volume, muted: i.muted, monitor: i.monitor })
            .collect();
        let scenes = serde_json::to_string_pretty(&persist::snapshot(&self.studio)).unwrap_or_default();
        let settings = serde_json::to_string_pretty(&self.settings).unwrap_or_default();
        if scenes != self.saved.0 {
            match persist::save_text("scenes.json", &scenes) {
                Ok(()) => self.saved.0 = scenes,
                Err(e) => self.status = format!("Couldn't save scenes: {e}"),
            }
        }
        if settings != self.saved.1 {
            match persist::save_text("settings.json", &settings) {
                Ok(()) => self.saved.1 = settings,
                Err(e) => self.status = format!("Couldn't save settings: {e}"),
            }
        }
    }

    // ---- streaming ----
    fn start_streaming(&mut self) {
        if !self.ffmpeg {
            self.status = "Streaming needs ffmpeg in PATH".into();
            return;
        }
        if self.settings.server.trim().is_empty() {
            self.status = "Set a stream server in Settings first".into();
            return;
        }
        let url = stream_target(&self.settings.server, &self.settings.key, self.settings.srt_latency_ms);
        let tap = self.mixer.start_tap();
        let (encoder, encoder_note) = self.chosen_encoder();
        let mut out = RtmpOutput::new(url, self.settings.bitrate).with_encoder(encoder).with_audio(Box::new(move |n| tap.pull(n)));
        let (w, h) = self.studio.size();
        if let Err(e) = out.start(w, h, self.fps()) {
            self.mixer.stop_tap();
            self.status = format!("Couldn't start stream: {e}");
            return;
        }
        self.streamer = Some(Streamer { out, started: Instant::now(), frames: 0, key: self.settings.key.trim().to_string() });
        self.status = match encoder_note {
            Some(n) => format!("Connecting to stream server... ({n})"),
            None => "Connecting to stream server...".into(),
        };
    }

    fn stop_streaming(&mut self, reason: Option<String>) {
        let Some(mut st) = self.streamer.take() else { return };
        let _ = st.out.stop();
        self.mixer.stop_tap();
        self.status = reason.unwrap_or_else(|| "Stream stopped".into());
    }

    // ---- sources ----
    fn open_add(&mut self, kind: NewKind) {
        let monitors = if kind == NewKind::Display { list_monitors() } else { Vec::new() };
        let windows = if kind == NewKind::Window { list_windows() } else { Vec::new() };
        let sel = monitors.iter().position(|m| m.primary).unwrap_or(0);
        self.add = Some(AddDialog {
            kind,
            name: kind.label().to_string(),
            monitors,
            windows,
            sel,
            path: String::new(),
            color: if kind == NewKind::Text { [255, 255, 255, 255] } else { [40, 120, 220, 255] },
            text: "Hello, world!".into(),
            size: 64.0,
            looping: true,
        });
    }

    fn create_source(&mut self, d: &AddDialog) -> Result<(), String> {
        let (cw, ch) = self.studio.size();
        let fps = 30;
        let source: Box<dyn Source> = match d.kind {
            NewKind::Display => {
                let m = d.monitors.get(d.sel).ok_or("No display selected")?;
                Box::new(CaptureSource::display(m, &d.name, fps))
            }
            NewKind::Window => {
                let w = d.windows.get(d.sel).ok_or("No window selected")?;
                Box::new(CaptureSource::window(w, &d.name, fps))
            }
            NewKind::Image => Box::new(ImageSource::open(&d.name, &d.path)?),
            NewKind::Media => {
                // A video with sound gets its own channel in the Audio Mixer.
                let sink = if probe_has_audio(&d.path) { Some(self.media_sink(&d.name)) } else { None };
                Box::new(CaptureSource::media(&d.name, &d.path, d.looping, sink)?)
            }
            NewKind::Color => Box::new(ColorSource::new(&d.name, cw, ch, d.color)),
            NewKind::Text => Box::new(TextSource::new(&d.name, &d.text, d.size, d.color)),
            NewKind::Pattern => Box::new(TestPatternSource::new(&d.name, cw / 3, ch / 3)),
        };
        // Size hint for capture sources whose first frame hasn't arrived yet.
        let hint = match d.kind {
            NewKind::Display => d.monitors.get(d.sel).map(|m| (m.width, m.height)),
            NewKind::Window => d.windows.get(d.sel).map(|w| (w.width, w.height)),
            _ => None,
        };
        let idx = self.studio.add_item(SceneItem::new(source));
        match d.kind {
            NewKind::Display | NewKind::Window | NewKind::Image | NewKind::Media => {
                let (w, h) = hint.unwrap_or_else(|| self.studio.edit_scene().items[idx].source.size());
                let item = &mut self.studio.edit_scene_mut().items[idx];
                // Fit large captures to the canvas; keep small images at 100%.
                item.scale = (cw as f32 / w as f32).min(ch as f32 / h as f32).min(1.0);
                item.x = ((cw as f32 - w as f32 * item.scale) / 2.0) as i32;
                item.y = ((ch as f32 - h as f32 * item.scale) / 2.0) as i32;
            }
            NewKind::Text => {
                let item = &mut self.studio.edit_scene_mut().items[idx];
                item.x = (cw / 12) as i32;
                item.y = (ch / 12) as i32;
            }
            NewKind::Pattern => {
                let item = &mut self.studio.edit_scene_mut().items[idx];
                item.x = (cw / 3) as i32;
                item.y = (ch / 3) as i32;
            }
            NewKind::Color => {}
        }
        Ok(())
    }

    fn apply(&mut self, a: Action) {
        let now = self.clock.elapsed().as_nanos() as u64;
        match a {
            Action::SelectScene(i) => self.studio.select_scene(i, now),
            Action::AddScene => {
                let n = self.studio.scenes.scenes.len() + 1;
                let name = if n == 2 { "Scene 2".to_string() } else { format!("Scene {n}") };
                let i = self.studio.add_scene(&name);
                self.studio.select_scene(i, now);
            }
            Action::RemoveScene(i) => { self.studio.remove_scene(i); }
            Action::Select(i) => self.studio.selected = Some(i),
            Action::RemoveItem(i) => self.studio.remove_item(i),
            Action::MoveItem(i, d) => self.studio.move_item(i, d),
            Action::OpenAdd(k) => self.open_add(k),
            Action::Transition => self.studio.take_transition(now),
        }
    }

    // ---- preview canvas with OBS-style selection, move and scale ----
    fn canvas_view(&mut self, ui: &mut egui::Ui, is_preview_tex: bool, editable: bool, title: &str) {
        let tex = if is_preview_tex { self.tex_preview.clone() } else { self.tex_program.clone() };
        let (cw, ch) = self.studio.size();
        let (cwf, chf) = (cw as f32, ch as f32);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new(title).strong());
            let avail = ui.available_size();
            let aspect = cwf / chf;
            let mut size = Vec2::new(avail.x, avail.x / aspect);
            if size.y > avail.y { size = Vec2::new(avail.y * aspect, avail.y); }
            size = size.max(Vec2::splat(32.0));
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, Color32::BLACK);
            if let Some(t) = &tex {
                painter.image(t.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }

            let to_screen = |cx: f32, cy: f32| Pos2::new(rect.min.x + cx / cwf * rect.width(), rect.min.y + cy / chf * rect.height());
            let to_canvas = |p: Pos2| ((p.x - rect.min.x) / rect.width() * cwf, (p.y - rect.min.y) / rect.height() * chf);

            // Selection outline + handles.
            let sel_bounds = self.studio.selected.and_then(|i| self.studio.item_bounds(i));
            if editable {
                if let Some((x, y, w, h)) = sel_bounds {
                    let r = Rect::from_min_max(to_screen(x, y), to_screen(x + w, y + h));
                    painter.rect_stroke(r, 0.0_f32, Stroke::new(2.0_f32, Color32::from_rgb(230, 40, 40)));
                    for c in [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()] {
                        painter.rect_filled(Rect::from_center_size(c, Vec2::splat(9.0)), 0.0, Color32::from_rgb(230, 40, 40));
                    }
                }
            }
            if !editable { return; }

            let handle_at = |p: Pos2, b: (f32, f32, f32, f32)| -> Option<Corner> {
                let (x, y, w, h) = b;
                let pts = [
                    (Corner::TL, to_screen(x, y)),
                    (Corner::TR, to_screen(x + w, y)),
                    (Corner::BL, to_screen(x, y + h)),
                    (Corner::BR, to_screen(x + w, y + h)),
                ];
                pts.into_iter().find(|(_, q)| q.distance(p) <= 9.0).map(|(c, _)| c)
            };

            if resp.drag_started() {
                if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
                    let (mx, my) = to_canvas(p);
                    let mut started = false;
                    if let (Some(i), Some(b)) = (self.studio.selected, sel_bounds) {
                        if let Some(corner) = handle_at(p, b) {
                            let (x, y, w, h) = b;
                            let anchor = match corner {
                                Corner::TL => (x + w, y + h),
                                Corner::TR => (x, y + h),
                                Corner::BL => (x + w, y),
                                Corner::BR => (x, y),
                            };
                            let src = self.studio.edit_scene().items[i].cropped_size();
                            self.drag = Some(Drag::Scale { corner, anchor, src: (src.0 as f32, src.1 as f32) });
                            started = true;
                        }
                    }
                    if !started {
                        match self.studio.hit_test(mx, my) {
                            Some(i) => {
                                self.studio.selected = Some(i);
                                let it = &self.studio.edit_scene().items[i];
                                self.drag = Some(Drag::Move { grab: (mx, my), start: (it.x, it.y) });
                            }
                            None => { self.studio.selected = None; self.drag = None; }
                        }
                    }
                }
            }
            if resp.dragged() {
                if let (Some(p), Some(i)) = (resp.interact_pointer_pos(), self.studio.selected) {
                    let (mx, my) = to_canvas(p);
                    if let Some(item) = self.studio.edit_scene_mut().items.get_mut(i) {
                        match &self.drag {
                            Some(Drag::Move { grab, start }) => {
                                item.x = start.0 + (mx - grab.0).round() as i32;
                                item.y = start.1 + (my - grab.1).round() as i32;
                            }
                            Some(Drag::Scale { corner, anchor, src }) => {
                                let dx = (mx - anchor.0).abs().max(8.0);
                                let s = (dx / src.0).clamp(0.05, 8.0);
                                let (nw, nh) = (src.0 * s, src.1 * s);
                                let left = matches!(corner, Corner::TL | Corner::BL);
                                let top = matches!(corner, Corner::TL | Corner::TR);
                                item.scale = s;
                                item.x = (if left { anchor.0 - nw } else { anchor.0 }).round() as i32;
                                item.y = (if top { anchor.1 - nh } else { anchor.1 }).round() as i32;
                            }
                            None => {}
                        }
                    }
                }
            }
            if resp.drag_stopped() { self.drag = None; }
            if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (mx, my) = to_canvas(p);
                    self.studio.selected = self.studio.hit_test(mx, my);
                }
            }
        });
    }

    // ---- docks ----
    fn dock_scenes(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.label(egui::RichText::new("Scenes").strong());
        let live = self.studio.scenes.active;
        let editing = self.studio.edit_index();
        egui::ScrollArea::vertical().id_source("scenes").max_height(ui.available_height() - 34.0).show(ui, |ui| {
            for (i, s) in self.studio.scenes.scenes.iter().enumerate() {
                let sel = if self.studio.studio_mode { i == editing } else { i == live };
                let mut label = s.name.clone();
                if self.studio.studio_mode && i == live { label.push_str("  (live)"); }
                let r = ui.add_sized([ui.available_width(), 20.0], egui::SelectableLabel::new(sel, label));
                if r.clicked() { actions.push(Action::SelectScene(i)); }
            }
        });
        ui.horizontal(|ui| {
            if ui.button("+").on_hover_text("Add scene").clicked() { actions.push(Action::AddScene); }
            if ui.button("-").on_hover_text("Remove scene").clicked() { actions.push(Action::RemoveScene(editing)); }
        });
    }

    fn dock_sources(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.label(egui::RichText::new("Sources").strong());
        let selected = self.studio.selected;
        let n = self.studio.edit_scene().items.len();
        egui::ScrollArea::vertical().id_source("sources").max_height(ui.available_height() - 34.0).show(ui, |ui| {
            if n == 0 {
                ui.weak("No sources. Click + to add one.");
            }
            for i in (0..n).rev() {
                let (name, kind) = {
                    let s = &self.studio.edit_scene().items[i].source;
                    (s.name().to_string(), s.kind())
                };
                ui.horizontal(|ui| {
                    let item = &mut self.studio.edit_scene_mut().items[i];
                    ui.checkbox(&mut item.visible, "").on_hover_text("Show / hide");
                    let r = ui.add_sized([ui.available_width(), 20.0], egui::SelectableLabel::new(selected == Some(i), if name == kind { name.clone() } else { format!("{name}   ({kind})") }));
                    if r.clicked() { actions.push(Action::Select(i)); }
                    if r.double_clicked() { self.show_props = true; }
                });
            }
        });
        ui.horizontal(|ui| {
            ui.menu_button("+", |ui| {
                for k in [NewKind::Display, NewKind::Window, NewKind::Image, NewKind::Media, NewKind::Text, NewKind::Color, NewKind::Pattern] {
                    if ui.button(k.label()).clicked() { actions.push(Action::OpenAdd(k)); ui.close_menu(); }
                }
            });
            if let Some(i) = selected {
                if ui.button("-").on_hover_text("Remove source").clicked() { actions.push(Action::RemoveItem(i)); }
                if ui.button("Properties").clicked() { self.show_props = true; }
                if ui.button("Up").clicked() { actions.push(Action::MoveItem(i, 1)); }
                if ui.button("Down").clicked() { actions.push(Action::MoveItem(i, -1)); }
            }
        });
    }

    fn dock_audio(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Audio Mixer").strong());
        if self.mixer.inputs.is_empty() {
            ui.weak("No audio input devices found.");
        }
        for inp in &mut self.mixer.inputs {
            ui.label(&inp.name);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 1.0_f32, Color32::from_rgb(0x12, 0x12, 0x14));
            let db: f32 = if inp.level > 0.0001 { 20.0 * inp.level.log10() } else { -60.0 };
            let frac: f32 = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            for (lo, hi, col) in [(-60.0_f32, -20.0_f32, Color32::from_rgb(60, 200, 90)), (-20.0, -9.0, Color32::from_rgb(230, 200, 50)), (-9.0, 0.0, Color32::from_rgb(225, 60, 50))] {
                let (a, b) = (((lo + 60.0) / 60.0) as f32, ((hi + 60.0) / 60.0) as f32);
                let end = frac.min(b);
                if end > a {
                    let seg = Rect::from_min_max(
                        Pos2::new(rect.min.x + rect.width() * a, rect.min.y),
                        Pos2::new(rect.min.x + rect.width() * end, rect.max.y),
                    );
                    p.rect_filled(seg, 0.0, col);
                }
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = 64.0;
                ui.add(egui::Slider::new(&mut inp.volume, 0.0..=1.5).show_value(false));
                ui.checkbox(&mut inp.muted, "Mute");
                ui.checkbox(&mut inp.monitor, "Monitor")
                    .on_hover_text("Also play this through your speakers. With Desktop Audio capture on, this doubles the sound in the stream.");
            });
        }
        for e in &self.mixer.errors { ui.colored_label(Color32::LIGHT_RED, e); }
    }

    fn dock_transitions(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Scene Transitions").strong());
        egui::ComboBox::from_id_source("transition_kind").selected_text(self.studio.transition_kind.label()).show_ui(ui, |ui| {
            for k in TransitionKind::ALL {
                ui.selectable_value(&mut self.studio.transition_kind, k, k.label());
            }
        });
        let mut ms = (self.studio.transition_ns / 1_000_000) as u32;
        let no_duration = matches!(self.studio.transition_kind, TransitionKind::Cut | TransitionKind::Stinger);
        ui.add_enabled_ui(!no_duration, |ui| {
            ui.horizontal(|ui| {
                ui.label("Duration");
                if ui.add(egui::DragValue::new(&mut ms).clamp_range(0..=5000).suffix(" ms")).changed() {
                    self.studio.transition_ns = ms as u64 * 1_000_000;
                }
            });
        });
        if self.studio.transition_kind == TransitionKind::Stinger {
            ui.add(
                egui::TextEdit::singleline(&mut self.studio.stinger_path)
                    .hint_text("stinger video (.mov/.webm/.mp4)")
                    .desired_width(ui.available_width()),
            );
            if ui.button("Browse...").clicked() {
                let start = self.studio.stinger_path.clone();
                self.open_browser(BrowseTarget::Stinger, &start, VIDEO_EXTS, false);
            }
            ui.horizontal(|ui| {
                ui.label("Cut point");
                ui.add(egui::DragValue::new(&mut self.studio.stinger_cut_ms).clamp_range(0..=20_000).suffix(" ms"));
            });
            ui.weak("Scene switches at the cut point; use a clip with alpha (.mov/.webm) to reveal it.");
        }
    }

    fn dock_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(egui::RichText::new("Controls").strong());
        let w = ui.available_width();
        if self.streamer.is_none() {
            let ready = self.ffmpeg && !self.settings.server.trim().is_empty();
            let hover = if !self.ffmpeg { "ffmpeg was not found in PATH" } else { "Set the server and stream key in Settings" };
            let r = ui.add_enabled(ready, egui::Button::new("Start Streaming").min_size(Vec2::new(w, 22.0)));
            if r.clicked() { self.start_streaming(); }
            if !ready { r.on_disabled_hover_text(hover); }
        } else if ui.add(egui::Button::new("Stop Streaming").fill(Color32::from_rgb(0x8f, 0x24, 0x24)).min_size(Vec2::new(w, 22.0))).clicked() {
            self.stop_streaming(None);
        }
        if self.recorder.is_none() {
            if ui.add(egui::Button::new("Start Recording").min_size(Vec2::new(w, 22.0))).clicked() { self.start_recording(); }
        } else if ui.add(egui::Button::new("Stop Recording").fill(Color32::from_rgb(0x8f, 0x24, 0x24)).min_size(Vec2::new(w, 22.0))).clicked() {
            self.stop_recording();
        }
        let label = if self.studio.studio_mode { "Studio Mode (on)" } else { "Studio Mode" };
        if ui.add(egui::Button::new(label).min_size(Vec2::new(w, 22.0))).clicked() {
            let on = !self.studio.studio_mode;
            self.studio.set_studio_mode(on);
        }
        if ui.add(egui::Button::new("Settings").min_size(Vec2::new(w, 22.0))).clicked() { self.show_settings = true; }
        if ui.add(egui::Button::new("Exit").min_size(Vec2::new(w, 22.0))).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    // ---- windows ----
    fn windows(&mut self, ctx: &egui::Context) {
        // A file chosen in the browser last frame.
        if let Some((target, p)) = self.browse_result.take() {
            match target {
                BrowseTarget::AddPath => {
                    if let Some(d) = self.add.as_mut() {
                        d.name = browse::stem(Path::new(&p));
                        d.path = p;
                    }
                }
                BrowseTarget::Stinger => self.studio.stinger_path = p,
                BrowseTarget::RecDir => self.settings.rec_dir = p,
                BrowseTarget::EffectPath(fi) => {
                    let sel = self.studio.selected;
                    let eff = sel.and_then(|i| self.studio.edit_scene_mut().items.get_mut(i)).and_then(|it| it.effects.get_mut(fi));
                    if let Some(e) = eff {
                        if let FilterSpec::Lut { path, .. } | FilterSpec::Mask { path, .. } = &mut e.spec {
                            *path = p;
                        }
                    }
                }
                BrowseTarget::Font => {
                    let sel = self.studio.selected;
                    if let Some(t) = sel.and_then(|i| self.studio.edit_scene_mut().items.get_mut(i)).and_then(|it| it.source.edit_text()) {
                        t.font_path = p;
                        t.update();
                    }
                }
            }
        }

        // File browser
        if let Some((target, mut b)) = self.browser.take() {
            let mut open = true;
            let mut navigate: Option<PathBuf> = None;
            let mut choose: Option<PathBuf> = None;
            let mut cancel = false;
            let title = if b.folder_mode { "Choose folder" } else { "Choose file" };
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
                .fixed_size([680.0, 400.0])
                .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Up").clicked() { b.up(); }
                    let r = ui.add(egui::TextEdit::singleline(&mut b.path_text).desired_width(ui.available_width()));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let p = PathBuf::from(b.path_text.trim());
                        if p.is_dir() { navigate = Some(p); }
                        else if p.is_file() && !b.folder_mode { choose = Some(p); }
                        else { b.error = Some("That folder doesn't exist".into()); }
                    }
                });
                if let Some(e) = &b.error { ui.colored_label(Color32::LIGHT_RED, e); }
                ui.separator();
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_min_width(110.0);
                        ui.label(egui::RichText::new("Places").strong());
                        for (name, p) in b.places.clone() {
                            if ui.selectable_label(b.dir == p, name).clicked() { navigate = Some(p); }
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().min_scrolled_height(280.0).max_height(280.0).auto_shrink([false, false]).show(ui, |ui| ui.vertical(|ui| {
                        if b.entries.is_empty() { ui.weak("(nothing to show here)"); }
                        for e in b.entries.clone() {
                            let label = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
                            let sel = b.selected.as_ref() == Some(&e.path);
                            let r = ui.selectable_label(sel, label);
                            if r.double_clicked() {
                                if e.is_dir { navigate = Some(e.path.clone()); }
                                else if !b.folder_mode { choose = Some(e.path.clone()); }
                            } else if r.clicked() && (!e.is_dir || b.folder_mode) {
                                b.selected = Some(e.path.clone());
                            }
                        }
                    }));
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let can = b.folder_mode || b.selected.is_some();
                    let label = if b.folder_mode { "Select this folder" } else { "Open" };
                    if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                        choose = if b.folder_mode { Some(b.selected.clone().unwrap_or_else(|| b.dir.clone())) } else { b.selected.clone() };
                    }
                    if ui.button("Cancel").clicked() { cancel = true; }
                    if let Some(s) = &b.selected { ui.weak(s.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()); }
                });
            });
            if let Some(d) = navigate { b.navigate(d); }
            if let Some(p) = choose {
                self.browse_result = Some((target, p.to_string_lossy().into_owned()));
            } else if open && !cancel {
                self.browser = Some((target, b));
            }
        }

        // Add-source dialog
        if let Some(mut d) = self.add.take() {
            let mut open = true;
            let mut done = None; // Some(true)=ok, Some(false)=cancel
            let mut browse_req = false;
            egui::Window::new(format!("Add {}", d.kind.label())).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.horizontal(|ui| { ui.label("Name"); ui.text_edit_singleline(&mut d.name); });
                match d.kind {
                    NewKind::Display => {
                        if d.monitors.is_empty() { ui.colored_label(Color32::LIGHT_RED, "No displays found."); }
                        egui::ComboBox::from_label("Display").selected_text(d.monitors.get(d.sel).map(|m| format!("{} ({}x{})", m.name, m.width, m.height)).unwrap_or_default()).show_ui(ui, |ui| {
                            for (i, m) in d.monitors.iter().enumerate() {
                                ui.selectable_value(&mut d.sel, i, format!("{} ({}x{})", m.name, m.width, m.height));
                            }
                        });
                    }
                    NewKind::Window => {
                        if d.windows.is_empty() { ui.colored_label(Color32::LIGHT_RED, "No capturable windows found."); }
                        let text = d.windows.get(d.sel).map(|w| format!("[{}] {}", w.app, w.title)).unwrap_or_default();
                        egui::ComboBox::from_label("Window").width(320.0).selected_text(text).show_ui(ui, |ui| {
                            for (i, w) in d.windows.iter().enumerate() {
                                ui.selectable_value(&mut d.sel, i, format!("[{}] {}", w.app, w.title));
                            }
                        });
                        if ui.button("Refresh").clicked() { d.windows = list_windows(); d.sel = 0; }
                    }
                    NewKind::Image | NewKind::Media => {
                        ui.horizontal(|ui| {
                            ui.label("File");
                            ui.text_edit_singleline(&mut d.path);
                            if ui.button("Browse...").clicked() { browse_req = true; }
                        });
                        if d.kind == NewKind::Media {
                            ui.checkbox(&mut d.looping, "Loop");
                            ui.weak("Plays the video; its sound appears in the Audio Mixer. You can also drop files onto the window.");
                        } else {
                            ui.weak("PNG or JPEG. You can also drop files onto the window.");
                        }
                    }
                    NewKind::Color => {
                        ui.horizontal(|ui| { ui.label("Color"); ui.color_edit_button_srgba_unmultiplied(&mut d.color); });
                    }
                    NewKind::Text => {
                        ui.label("Text");
                        ui.add(egui::TextEdit::multiline(&mut d.text).desired_rows(3).desired_width(300.0));
                        ui.add(egui::Slider::new(&mut d.size, 12.0..=300.0).text("Size (px)"));
                        ui.horizontal(|ui| { ui.label("Color"); ui.color_edit_button_srgba_unmultiplied(&mut d.color); });
                    }
                    NewKind::Pattern => {}
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() { done = Some(true); }
                    if ui.button("Cancel").clicked() { done = Some(false); }
                });
            });
            if browse_req {
                let exts = if d.kind == NewKind::Media { VIDEO_EXTS } else { IMAGE_EXTS };
                self.open_browser(BrowseTarget::AddPath, &d.path, exts, false);
            }
            match done {
                Some(true) => {
                    if let Err(e) = self.create_source(&d) { self.status = format!("Couldn't add source: {e}"); }
                }
                Some(false) => {}
                None => { if open { self.add = Some(d); } }
            }
        }

        // Keyboard shortcuts
        if self.show_keys {
            let mut open = true;
            egui::Window::new("Keyboard Shortcuts").open(&mut open).resizable(false).show(ctx, |ui| {
                egui::Grid::new("keys").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                    for (k, what) in [
                        ("F9", "Start / stop recording"),
                        ("F10", "Start / stop streaming"),
                        ("Ctrl+M", "Toggle Studio Mode"),
                        ("Ctrl+Enter", "Transition (Studio Mode)"),
                        ("Ctrl+1 ... Ctrl+9", "Select scene 1-9"),
                        ("Delete", "Remove selected source"),
                        ("Arrow keys", "Nudge selected source (Shift = 10 px)"),
                    ] {
                        ui.monospace(k);
                        ui.label(what);
                        ui.end_row();
                    }
                });
                ui.separator();
                ui.weak("Shortcuts work while this window is focused (Cmd instead of Ctrl on macOS).");
            });
            if !open { self.show_keys = false; }
        }

        // Properties
        if self.show_props {
            let mut open = true;
            let sel = self.studio.selected;
            let (cw, ch) = self.studio.size();
            let mut fit = None;
            let mut want_font_browse: Option<String> = None;
            let mut want_effect_browse: Option<(usize, bool, String)> = None;
            egui::Window::new("Properties").open(&mut open).resizable(false).show(ctx, |ui| {
                match sel.and_then(|i| self.studio.edit_scene_mut().items.get_mut(i).map(|it| (i, it))) {
                    Some((i, item)) => {
                        let mut name = item.source.name().to_string();
                        ui.horizontal(|ui| {
                            ui.label("Name");
                            if ui.text_edit_singleline(&mut name).changed() { item.source.set_name(&name); }
                        });
                        ui.label(format!("Type: {}", item.source.kind()));
                        let (sw, sh) = item.source.size();
                        ui.label(format!("Source size: {sw} x {sh}"));
                        ui.separator();
                        ui.label(egui::RichText::new("Transform").strong());
                        ui.horizontal(|ui| {
                            ui.label("Position");
                            ui.add(egui::DragValue::new(&mut item.x).clamp_range(-(cw as i32 * 2)..=cw as i32 * 2));
                            ui.add(egui::DragValue::new(&mut item.y).clamp_range(-(ch as i32 * 2)..=ch as i32 * 2));
                        });
                        ui.add(egui::Slider::new(&mut item.scale, 0.05..=8.0).logarithmic(true).text("Scale"));
                        ui.add(egui::Slider::new(&mut item.opacity, 0.0..=1.0).text("Opacity"));
                        ui.separator();
                        ui.label(egui::RichText::new("Crop (source pixels)").strong());
                        for (idx, label) in ["Left", "Top", "Right", "Bottom"].iter().enumerate() {
                            let max = if idx % 2 == 0 { sw } else { sh }.saturating_sub(1);
                            ui.add(egui::Slider::new(&mut item.crop[idx], 0..=max).text(*label));
                        }
                        if ui.button("Fit to screen").clicked() { fit = Some(i); }
                        ui.separator();
                        ui.label(egui::RichText::new("Effects").strong());
                        ui.add(egui::Slider::new(&mut item.brightness, 0.0..=2.0).text("Brightness"));
                        ui.checkbox(&mut item.grayscale, "Grayscale");
                        ui.separator();
                        ui.label(egui::RichText::new("Filters").strong());
                        let (mut remove, mut shift): (Option<usize>, Option<(usize, i32)>) = (None, None);
                        let count = item.effects.len();
                        for (fi, e) in item.effects.iter_mut().enumerate() {
                            ui.push_id(fi, |ui| {
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut e.enabled, e.spec.label());
                                    if fi > 0 && ui.small_button("Up").clicked() { shift = Some((fi, -1)); }
                                    if fi + 1 < count && ui.small_button("Down").clicked() { shift = Some((fi, 1)); }
                                    if ui.small_button("Remove").clicked() { remove = Some(fi); }
                                });
                                ui.indent("params", |ui| match &mut e.spec {
                                    FilterSpec::ChromaKey { key, similarity, smoothness, spill } => {
                                        ui.horizontal(|ui| {
                                            ui.label("Key color");
                                            ui.color_edit_button_srgb(key);
                                            if ui.small_button("Green").clicked() { *key = [0, 255, 0]; }
                                            if ui.small_button("Blue").clicked() { *key = [0, 0, 255]; }
                                        });
                                        ui.add(egui::Slider::new(similarity, 0.0..=1.0).text("Similarity"));
                                        ui.add(egui::Slider::new(smoothness, 0.0..=0.5).text("Smoothness"));
                                        ui.add(egui::Slider::new(spill, 0.0..=1.0).text("Spill reduction"));
                                    }
                                    FilterSpec::Blur { radius } => {
                                        ui.add(egui::Slider::new(radius, 0..=64).text("Radius (px)"));
                                    }
                                    FilterSpec::ColorCorrection { brightness, contrast, saturation, gamma } => {
                                        ui.add(egui::Slider::new(brightness, -1.0..=1.0).text("Brightness"));
                                        ui.add(egui::Slider::new(contrast, 0.0..=3.0).text("Contrast"));
                                        ui.add(egui::Slider::new(saturation, 0.0..=3.0).text("Saturation"));
                                        ui.add(egui::Slider::new(gamma, 0.3..=3.0).text("Gamma"));
                                    }
                                    FilterSpec::Sharpen { amount } => {
                                        ui.add(egui::Slider::new(amount, 0.0..=3.0).text("Amount"));
                                    }
                                    FilterSpec::Lut { path, amount } => {
                                        ui.horizontal(|ui| {
                                            ui.add(egui::TextEdit::singleline(path).hint_text(".cube file").desired_width(170.0));
                                            if ui.button("Browse...").clicked() { want_effect_browse = Some((fi, true, path.clone())); }
                                        });
                                        ui.add(egui::Slider::new(amount, 0.0..=1.0).text("Amount"));
                                    }
                                    FilterSpec::Mask { path, mode } => {
                                        ui.horizontal(|ui| {
                                            ui.add(egui::TextEdit::singleline(path).hint_text("mask image (PNG/JPEG)").desired_width(170.0));
                                            if ui.button("Browse...").clicked() { want_effect_browse = Some((fi, false, path.clone())); }
                                        });
                                        egui::ComboBox::from_label("Mode")
                                            .selected_text(match mode { MaskMode::Alpha => "Alpha", MaskMode::Luma => "Luma (white shows)", MaskMode::InvertedLuma => "Inverted luma (black shows)" })
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(mode, MaskMode::Alpha, "Alpha");
                                                ui.selectable_value(mode, MaskMode::Luma, "Luma (white shows)");
                                                ui.selectable_value(mode, MaskMode::InvertedLuma, "Inverted luma (black shows)");
                                            });
                                    }
                                    FilterSpec::Scroll { x, y } => {
                                        ui.add(egui::Slider::new(x, -600.0..=600.0).text("Horizontal (px/s)"));
                                        ui.add(egui::Slider::new(y, -600.0..=600.0).text("Vertical (px/s)"));
                                    }
                                    FilterSpec::Flip { horizontal, vertical } => {
                                        ui.checkbox(horizontal, "Horizontal");
                                        ui.checkbox(vertical, "Vertical");
                                    }
                                });
                            });
                        }
                        if let Some(i) = remove { item.effects.remove(i); }
                        if let Some((i, d)) = shift { item.effects.swap(i, (i as i32 + d) as usize); }
                        ui.menu_button("+ Add filter", |ui| {
                            for spec in [FilterSpec::chroma_key(), FilterSpec::blur(), FilterSpec::color_correction(), FilterSpec::sharpen(), FilterSpec::lut(), FilterSpec::mask(), FilterSpec::scroll(), FilterSpec::flip()] {
                                if ui.button(spec.label()).clicked() { item.effects.push(Effect::new(spec)); ui.close_menu(); }
                            }
                        });
                        if let Some(t) = item.source.edit_text() {
                            ui.separator();
                            ui.label(egui::RichText::new("Text").strong());
                            let mut changed = ui.add(egui::TextEdit::multiline(&mut t.text).desired_rows(3).desired_width(300.0)).changed();
                            changed |= ui.add(egui::Slider::new(&mut t.size, 12.0..=300.0).text("Size (px)")).changed();
                            ui.horizontal(|ui| {
                                ui.label("Color");
                                changed |= ui.color_edit_button_srgba_unmultiplied(&mut t.color).changed();
                                ui.label("Background");
                                changed |= ui.color_edit_button_srgba_unmultiplied(&mut t.background).changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("Font file");
                                changed |= ui.add(egui::TextEdit::singleline(&mut t.font_path).hint_text("optional .ttf/.otf path").desired_width(220.0)).changed();
                            });
                            if ui.button("Browse for font...").clicked() { want_font_browse = Some(t.font_path.clone()); }
                            if t.font_failed() { ui.colored_label(Color32::YELLOW, "Couldn't load that font; using the default."); }
                            if changed { t.update(); }
                        }
                    }
                    None => { ui.label("Select a source first."); }
                }
            });
            if let Some(i) = fit { self.studio.fit_to_screen(i); }
            if let Some((fi, is_lut, start)) = want_effect_browse {
                let exts: &[&str] = if is_lut { &["cube"] } else { IMAGE_EXTS };
                self.open_browser(BrowseTarget::EffectPath(fi), &start, exts, false);
            }
            if let Some(start) = want_font_browse { self.open_browser(BrowseTarget::Font, &start, FONT_EXTS, false); }
            if !open { self.show_props = false; }
        }

        // Settings
        if self.show_settings {
            let mut open = true;
            let mut canvas_changed = false;
            let mut want_rec_browse = false;
            let mut gpu_changed = false;
            egui::Window::new("Settings").open(&mut open).resizable(false).vscroll(true).default_height(560.0).show(ctx, |ui| {
                ui.label(egui::RichText::new("Video").strong());
                let name = |(w, h): (u32, u32)| format!("{w}x{h}");
                egui::ComboBox::from_label("Canvas / output resolution").selected_text(name(CANVAS_SIZES[self.settings.canvas])).show_ui(ui, |ui| {
                    for (i, s) in CANVAS_SIZES.iter().enumerate() {
                        if ui.selectable_value(&mut self.settings.canvas, i, name(*s)).changed() { canvas_changed = true; }
                    }
                });
                egui::ComboBox::from_label("Frame rate").selected_text(format!("{} fps", FPS_CHOICES[self.settings.fps])).show_ui(ui, |ui| {
                    for (i, f) in FPS_CHOICES.iter().enumerate() { ui.selectable_value(&mut self.settings.fps, i, format!("{f} fps")); }
                });
                if ui.checkbox(&mut self.settings.gpu, "Composite on the GPU (experimental)").changed() { gpu_changed = true; }
                ui.weak("Draws scenes on your graphics card; falls back to the CPU automatically if it can't.");
                ui.separator();
                ui.label(egui::RichText::new("Stream").strong());
                egui::ComboBox::from_label("Service").selected_text(SERVICES[self.settings.service].0).show_ui(ui, |ui| {
                    for (i, (name, url)) in SERVICES.iter().enumerate() {
                        if ui.selectable_value(&mut self.settings.service, i, *name).clicked() && !url.is_empty() {
                            self.settings.server = url.to_string();
                        }
                    }
                });
                ui.horizontal(|ui| { ui.label("Server"); ui.add(egui::TextEdit::singleline(&mut self.settings.server).desired_width(280.0)); });
                ui.horizontal(|ui| { ui.label("Stream key"); ui.add(egui::TextEdit::singleline(&mut self.settings.key).password(true).desired_width(280.0)); });
                ui.horizontal(|ui| {
                    ui.label("Video bitrate");
                    ui.add(egui::DragValue::new(&mut self.settings.bitrate).clamp_range(500..=20000).suffix(" kbps"));
                });
                ui.weak("The server URL is saved. The stream key is kept in memory only, so enter it each launch.");
                ui.horizontal(|ui| {
                    ui.label("SRT latency");
                    ui.add(egui::DragValue::new(&mut self.settings.srt_latency_ms).clamp_range(20..=8000).suffix(" ms"));
                });
                ui.weak("SRT: use srt://host:port; the key is sent as the SRT streamid and the latency above is applied for you. Higher latency survives worse networks.");
                ui.separator();
                ui.label(egui::RichText::new("Hotkeys (work even when another app is focused)").strong());
                if !self.hotkeys.available() {
                    ui.colored_label(Color32::YELLOW, "Global key state isn't available here (e.g. Wayland); only in-window shortcuts work.");
                }
                egui::Grid::new("hotkey_grid").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
                    for a in HotkeyAction::ALL {
                        ui.label(a.label());
                        let text = if self.capturing == Some(a) {
                            "Press keys...".to_string()
                        } else {
                            display_combo(&self.hotkeys.binding(a).unwrap_or_default())
                        };
                        ui.monospace(text);
                        ui.horizontal(|ui| {
                            if self.capturing == Some(a) {
                                if ui.button("Cancel").clicked() { self.capturing = None; self.hotkeys.cancel_capture(); }
                            } else {
                                if ui.button("Set").clicked() { self.capturing = Some(a); self.hotkeys.begin_capture(); }
                                if ui.button("Clear").clicked() {
                                    self.settings.hotkeys.remove(a.id());
                                    self.hotkeys.set_binding(a, Vec::new());
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
                ui.weak("Click Set, then press and release the combination (e.g. Ctrl+Alt+R). In-window F9/F10 keep working for actions without a hotkey.");
                ui.separator();
                ui.label(egui::RichText::new("Encoding").strong());
                let probed = self.encoders.lock().unwrap().clone();
                let usable = |e: VideoEncoder| e == VideoEncoder::X264 || probed.as_ref().map_or(false, |l| l.contains(&(e, true)));
                let current = VideoEncoder::from_id(&self.settings.encoder).unwrap_or(VideoEncoder::X264);
                egui::ComboBox::from_label("Video encoder").selected_text(current.label()).show_ui(ui, |ui| {
                    for e in VideoEncoder::ALL {
                        let note = match &probed {
                            None if e != VideoEncoder::X264 => " (checking...)",
                            Some(_) if !usable(e) => " (not available)",
                            _ => "",
                        };
                        ui.add_enabled_ui(usable(e) || e == current, |ui| {
                            ui.selectable_value(&mut self.settings.encoder, e.id().to_string(), format!("{}{note}", e.label()));
                        });
                    }
                });
                ui.weak("Hardware encoders (NVIDIA / Intel / AMD / Apple) use far less CPU. Each is test-encoded on startup, and only those that work can be picked.");
                ui.add(egui::Slider::new(&mut self.settings.rec_quality, 14..=32).text("Recording quality (lower = better, bigger files)"));
                egui::ComboBox::from_label("Recording format")
                    .selected_text(self.settings.rec_format.to_uppercase())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.settings.rec_format, "mp4".to_string(), "MP4");
                        ui.selectable_value(&mut self.settings.rec_format, "mkv".to_string(), "MKV");
                    });
                ui.weak("Recordings are written crash-safe and finished into the final file when you stop.");
                ui.separator();
                ui.label(egui::RichText::new("Output").strong());
                ui.horizontal(|ui| {
                    ui.label("Recording folder");
                    ui.text_edit_singleline(&mut self.settings.rec_dir);
                    if ui.button("Browse...").clicked() { want_rec_browse = true; }
                });
                if self.ffmpeg { ui.label(format!("Recording as {} (H.264 via ffmpeg)", self.settings.rec_format.to_uppercase())); }
                else { ui.colored_label(Color32::YELLOW, "ffmpeg not found in PATH: recording falls back to raw .y4m"); }
                ui.weak("Resolution/fps changes apply to the next recording.");
                ui.weak(format!("Scenes and settings are saved automatically in {}", persist::config_dir().display()));
            });
            if gpu_changed {
                self.apply_compositor();
            }
            if want_rec_browse {
                let start = self.settings.rec_dir.clone();
                self.open_browser(BrowseTarget::RecDir, &start, &[], true);
            }
            if canvas_changed && self.recorder.is_none() && self.streamer.is_none() {
                let (w, h) = CANVAS_SIZES[self.settings.canvas];
                self.studio.set_canvas(w, h);
            } else if canvas_changed {
                self.settings.canvas = CANVAS_SIZES.iter().position(|&s| s == self.studio.size()).unwrap_or(1);
                self.status = "Stop recording/streaming before changing the canvas".into();
            }
            if !open {
                self.show_settings = false;
                self.capturing = None;
                self.hotkeys.cancel_capture();
            }
        }
    }
}

fn set_tex(ctx: &egui::Context, slot: &mut Option<egui::TextureHandle>, name: &str, f: &rustin_core::Frame) {
    // Large canvases: upload a half-resolution copy. The preview is shown in a window-sized
    // area anyway, and this cuts the per-frame conversion/upload cost by 4x.
    let img = if f.width > 1600 {
        let (w, h) = ((f.width / 2) as usize, (f.height / 2) as usize);
        let mut px = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let row = (y * 2) * f.width as usize * 4;
            for x in 0..w {
                let i = row + x * 8;
                px.extend_from_slice(&f.data[i..i + 4]);
            }
        }
        egui::ColorImage::from_rgba_unmultiplied([w, h], &px)
    } else {
        egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.data)
    };
    match slot {
        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, img, egui::TextureOptions::LINEAR)),
    }
}

impl eframe::App for RustinApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut actions: Vec<Action> = Vec::new();
        let t_ns = self.clock.elapsed().as_nanos() as u64;
        let fps = self.fps();
        self.studio.fps = fps;

        // ---- render ----
        let t0 = Instant::now();
        let program = self.studio.render_program(t_ns);
        set_tex(ctx, &mut self.tex_program, "program", &program);
        if self.studio.studio_mode {
            let pv = self.studio.render_preview(t_ns);
            set_tex(ctx, &mut self.tex_preview, "preview", &pv);
        }
        self.render_ms = self.render_ms * 0.9 + t0.elapsed().as_secs_f32() * 1000.0 * 0.1;
        self.frame_times.push_back(Instant::now());
        while self.frame_times.front().map_or(false, |t| t.elapsed().as_secs_f32() > 1.0) { self.frame_times.pop_front(); }

        // ---- record (paced by wall clock) ----
        let mut rec_error = None;
        if let Some(r) = &mut self.recorder {
            if r.out.uses_wallclock() {
                // ffmpeg timestamps frames by arrival and fills/drops to a constant rate, so each
                // rendered frame is pushed once; the video stays in real time however slow we are.
                match r.out.push_video(&program) {
                    Ok(()) => r.frames += 1,
                    Err(e) => rec_error = Some(e.to_string()),
                }
            } else {
                // Raw .y4m has no timestamps: repeat frames to keep a fixed pace.
                let target = (r.started.elapsed().as_secs_f64() * fps as f64) as u64;
                let mut pushed = 0;
                while r.frames < target && pushed < 4 {
                    if let Err(e) = r.out.push_video(&program) { rec_error = Some(e.to_string()); break; }
                    r.frames += 1;
                    pushed += 1;
                }
            }
        }
        if let Some(e) = rec_error { self.stop_recording(); self.status = format!("Recording stopped: {e}"); }

        // ---- stream (paced by wall clock; a slow network drops frames, never blocks the UI) ----
        let mut stream_ended = None;
        let mut stream_live = false;
        if let Some(st) = &mut self.streamer {
            stream_live = st.started.elapsed().as_secs() >= 2;
            if st.out.push_video(&program).is_ok() {
                st.frames += 1;
            }
            let stats = st.out.stats();
            if !stats.alive && st.started.elapsed().as_millis() > 300 {
                let mut msg = stats.last_error.clone();
                if !st.key.is_empty() { msg = msg.replace(&st.key, "***"); }
                stream_ended = Some(if msg.is_empty() { "Stream ended".to_string() } else { format!("Stream ended: {msg}") });
            }
        }
        if let Some(msg) = stream_ended { self.stop_streaming(Some(msg)); }
        else if stream_live && self.status.starts_with("Connecting") { self.status = "Streaming".into(); }
        self.mixer.update_levels();
        self.ensure_monitor();
        self.autosave(false);
        if let Some(e) = self.studio.last_error.take() { self.status = e; }
        if let Some(e) = self.studio.comp.backend_error.take() {
            self.settings.gpu = false;
            self.status = e;
        }

        // ---- shortcuts (work while the window is focused and no text box is active) ----
        if !ctx.wants_keyboard_input() {
            use egui::Key;
            let (rec, stream, studio_toggle, transition, scene) = ctx.input(|i| {
                let cmd = i.modifiers.command;
                let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
                let scene = if cmd { digits.iter().position(|k| i.key_pressed(*k)) } else { None };
                (i.key_pressed(Key::F9), i.key_pressed(Key::F10), cmd && i.key_pressed(Key::M), cmd && i.key_pressed(Key::Enter), scene)
            });
            // An action with a system-wide binding is handled there, so it never fires twice.
            if rec && !self.hotkeys.is_bound(HotkeyAction::ToggleRecord) { self.run_hotkey(HotkeyAction::ToggleRecord, &mut actions); }
            if stream && !self.hotkeys.is_bound(HotkeyAction::ToggleStream) { self.run_hotkey(HotkeyAction::ToggleStream, &mut actions); }
            if studio_toggle && !self.hotkeys.is_bound(HotkeyAction::ToggleStudioMode) { self.run_hotkey(HotkeyAction::ToggleStudioMode, &mut actions); }
            if transition && !self.hotkeys.is_bound(HotkeyAction::Transition) { actions.push(Action::Transition); }
            if let Some(n) = scene { actions.push(Action::SelectScene(n)); }
        }

        // ---- files dropped onto the window ----
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for p in dropped { self.handle_drop(&p); }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_overlay")));
            let r = ctx.screen_rect();
            painter.rect_filled(r, 0.0_f32, Color32::from_black_alpha(160));
            painter.text(r.center(), egui::Align2::CENTER_CENTER, "Drop an image or video to add it as a source", egui::FontId::proportional(26.0), Color32::WHITE);
        }

        // ---- system-wide hotkeys (work even when another app is focused) ----
        for ev in self.hotkeys.poll() {
            match ev {
                HotkeyEvent::Triggered(a) => self.run_hotkey(a, &mut actions),
                HotkeyEvent::Captured(combo) => {
                    if let Some(a) = self.capturing.take() {
                        self.status = format!("{}: {}", a.label(), display_combo(&combo));
                        self.settings.hotkeys.insert(a.id().to_string(), combo_to_strings(&combo));
                        self.hotkeys.set_binding(a, combo);
                    }
                }
            }
        }

        // ---- keyboard: Delete / arrows act on the selected source ----
        if !ctx.wants_keyboard_input() {
            if let Some(i) = self.studio.selected {
                let (del, dx, dy) = ctx.input(|inp| {
                    let step = if inp.modifiers.shift { 10 } else { 1 };
                    let dx = (inp.key_pressed(egui::Key::ArrowRight) as i32 - inp.key_pressed(egui::Key::ArrowLeft) as i32) * step;
                    let dy = (inp.key_pressed(egui::Key::ArrowDown) as i32 - inp.key_pressed(egui::Key::ArrowUp) as i32) * step;
                    (inp.key_pressed(egui::Key::Delete), dx, dy)
                });
                if del { actions.push(Action::RemoveItem(i)); }
                if let Some(it) = self.studio.edit_scene_mut().items.get_mut(i) { it.x += dx; it.y += dy; }
            }
        }

        // ---- menu bar ----
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Settings").clicked() { self.show_settings = true; ui.close_menu(); }
                    if ui.button("Exit").clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                });
                ui.menu_button("View", |ui| {
                    let mut sm = self.studio.studio_mode;
                    if ui.checkbox(&mut sm, "Studio Mode").changed() { self.studio.set_studio_mode(sm); }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Keyboard Shortcuts").clicked() { self.show_keys = true; ui.close_menu(); }
                    ui.separator();
                    ui.label("Rustin Studio 1.0");
                    ui.label("GPL-2.0-or-later");
                });
            });
        });

        // ---- status bar (outermost bottom panel) ----
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(st) = &self.streamer {
                    let s = st.started.elapsed().as_secs();
                    ui.colored_label(Color32::from_rgb(235, 70, 70), format!("LIVE: {:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60));
                    let d = st.out.stats().dropped_frames;
                    if d > 0 { ui.colored_label(Color32::YELLOW, format!("dropped {d}")); }
                } else {
                    ui.weak("LIVE: 00:00:00");
                }
                ui.separator();
                if let Some(r) = &self.recorder {
                    let s = r.started.elapsed().as_secs();
                    ui.colored_label(Color32::from_rgb(235, 70, 70), format!("REC: {:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60));
                } else {
                    ui.weak("REC: 00:00:00");
                }
                ui.separator();
                ui.label(format!("Render: {:.1} ms", self.render_ms));
                ui.separator();
                ui.label(format!("{:.1} / {} fps", self.frame_times.len() as f32, fps));
                ui.separator();
                let (w, h) = self.studio.size();
                ui.label(format!("{w}x{h}"));
                ui.separator();
                ui.label(self.studio.comp.backend.as_ref().map(|b| b.name()).unwrap_or_else(|| "CPU".into()));
                ui.separator();
                ui.label(&self.status);
            });
        });

        // ---- docks ----
        egui::TopBottomPanel::bottom("docks").resizable(false).exact_height(190.0).show(ctx, |ui| {
            ui.columns(5, |cols| {
                self.dock_scenes(&mut cols[0], &mut actions);
                self.dock_sources(&mut cols[1], &mut actions);
                self.dock_audio(&mut cols[2]);
                self.dock_transitions(&mut cols[3]);
                self.dock_controls(&mut cols[4], ctx);
            });
        });

        // ---- center ----
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.studio.studio_mode {
                ui.columns(2, |cols| {
                    self.canvas_view(&mut cols[0], true, true, "Preview");
                    self.canvas_view(&mut cols[1], false, false, "Program");
                });
                ui.vertical_centered(|ui| {
                    if ui.add(egui::Button::new("Transition").min_size(Vec2::new(140.0, 26.0))).clicked() { actions.push(Action::Transition); }
                });
            } else {
                self.canvas_view(ui, false, true, "");
            }
        });

        self.windows(ctx);
        for a in actions { self.apply(a); }
        ctx.request_repaint_after(std::time::Duration::from_millis(1000 / fps as u64));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.autosave(true);
        self.stop_streaming(None);
        self.stop_recording();
    }
}

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let display = args.iter().any(|a| a == "--display");
    let stream_server = args.iter().position(|a| a == "--stream-url").and_then(|i| args.get(i + 1)).cloned();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]).with_title("Rustin Studio"),
        ..Default::default()
    };
    eframe::run_native(
        "Rustin Studio",
        options,
        Box::new(move |cc| {
            apply_obs_theme(&cc.egui_ctx);
            Box::new(RustinApp::new(demo, display, stream_server))
        }),
    )
}
