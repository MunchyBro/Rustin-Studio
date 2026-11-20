//! GUI-independent studio state: scenes, selection, studio mode, transitions.

pub mod browse;
pub mod persist;

use rustin_output::{ffmpeg_available, StingerStream};
use rustin_core::{
    source::{BouncingBoxSource, ColorSource, TestPatternSource},
    transition::{self, TransitionKind},
    Compositor, Frame, Scene, SceneCollection, SceneItem,
};

/// Combine a server URL and stream key into the full RTMP target.
/// RTMP appends the key as a path segment; SRT passes it as the `streamid` parameter.
pub fn stream_url(server: &str, key: &str) -> String {
    let server = server.trim();
    let key = key.trim();
    if server.to_ascii_lowercase().starts_with("srt://") {
        if key.is_empty() {
            return server.to_string();
        }
        let sep = if server.contains('?') { '&' } else { '?' };
        return format!("{server}{sep}streamid={key}");
    }
    let server = server.trim_end_matches('/');
    if key.is_empty() { server.to_string() } else { format!("{server}/{key}") }
}

/// Full target URL for the encoder. For SRT this also applies the latency setting: ffmpeg's SRT
/// `latency` option is in **microseconds** (verified), while users think in milliseconds, so a
/// value typed into the URL is respected as-is and otherwise the ms setting is converted.
pub fn stream_target(server: &str, key: &str, srt_latency_ms: u32) -> String {
    let url = stream_url(server, key);
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("srt://") && !lower.contains("latency=") {
        let sep = if url.contains('?') { '&' } else { '?' };
        return format!("{url}{sep}latency={}", srt_latency_ms as u64 * 1000);
    }
    url
}

struct Transition {
    from: usize,
    start_ns: u64,
    stinger: Option<StingerStream>,
    /// Studio clock time at which the stinger's first frame arrived (its playback clock starts here).
    stinger_clock: Option<u64>,
}

pub struct Studio {
    pub scenes: SceneCollection,
    pub comp: Compositor,
    pub selected: Option<usize>,
    pub transition_ns: u64,
    pub transition_kind: TransitionKind,
    /// Video file for the Stinger transition, and when (ms into the clip) the scene actually switches.
    pub stinger_path: String,
    pub stinger_cut_ms: u32,
    /// Output frame rate; stingers are decoded at this rate.
    pub fps: u32,
    /// Last problem worth telling the user about (e.g. stinger couldn't play); the UI takes it.
    pub last_error: Option<String>,
    /// OBS "Studio Mode": edits go to the preview scene; Transition sends it live.
    pub studio_mode: bool,
    pub preview: usize,
    transition: Option<Transition>,
}

impl Studio {
    /// One empty scene, like a fresh OBS install.
    pub fn new(width: u32, height: u32) -> Self {
        let mut s = Self {
            scenes: SceneCollection::default(),
            comp: Compositor::new(width, height),
            selected: None,
            transition_ns: 300_000_000,
            transition_kind: TransitionKind::Fade,
            stinger_path: String::new(),
            stinger_cut_ms: 500,
            fps: 30,
            last_error: None,
            studio_mode: false,
            preview: 0,
            transition: None,
        };
        s.scenes.add_scene(Scene::new("Scene"));
        s
    }

    /// Two scenes filled with generated sources (for trying the UI without capture).
    pub fn demo(width: u32, height: u32) -> Self {
        let mut s = Self::new(width, height);
        s.add_item(SceneItem::new(Box::new(TestPatternSource::new("Test Pattern", width, height))));
        s.add_item(SceneItem::new(Box::new(BouncingBoxSource::new("Bouncing Box", width, height, height / 10, [255, 220, 0, 255]))));
        let i = s.add_scene("Scene 2");
        s.scenes.active = i;
        s.add_item(SceneItem::new(Box::new(ColorSource::new("Color Source", width, height, [30, 30, 60, 255]))));
        s.add_item(
            SceneItem::new(Box::new(TestPatternSource::new("Test Pattern 2", width / 3, height / 3)))
                .at((width / 3) as i32, (height / 3) as i32),
        );
        s.scenes.active = 0;
        s.selected = None;
        s
    }

    pub fn size(&self) -> (u32, u32) { (self.comp.width, self.comp.height) }

    pub fn set_canvas(&mut self, width: u32, height: u32) {
        self.comp.width = width;
        self.comp.height = height;
    }

    pub fn active_scene(&self) -> &Scene { &self.scenes.scenes[self.scenes.active] }

    /// Index of the scene the user is currently editing.
    pub fn edit_index(&self) -> usize {
        if self.studio_mode { self.preview.min(self.scenes.scenes.len() - 1) } else { self.scenes.active }
    }
    pub fn edit_scene(&self) -> &Scene { &self.scenes.scenes[self.edit_index()] }
    pub fn edit_scene_mut(&mut self) -> &mut Scene {
        let i = self.edit_index();
        &mut self.scenes.scenes[i]
    }

    pub fn add_scene(&mut self, name: &str) -> usize {
        self.scenes.add_scene(Scene::new(name))
    }

    /// Removes a scene; the last remaining scene can't be removed.
    pub fn remove_scene(&mut self, idx: usize) -> bool {
        let n = self.scenes.scenes.len();
        if n <= 1 || idx >= n {
            return false;
        }
        self.scenes.scenes.remove(idx);
        let fix = |i: &mut usize| {
            if *i >= n - 1 { *i = n - 2 } else if idx < *i { *i -= 1 }
        };
        fix(&mut self.scenes.active);
        fix(&mut self.preview);
        self.transition = None;
        self.selected = None;
        true
    }

    /// Switch the program scene, cross-fading from the current one.
    pub fn switch_to(&mut self, idx: usize, now_ns: u64) {
        if idx >= self.scenes.scenes.len() || idx == self.scenes.active {
            return;
        }
        if self.transition_kind != TransitionKind::Cut {
            let stinger = if self.transition_kind == TransitionKind::Stinger { self.open_stinger() } else { None };
            self.transition = Some(Transition { from: self.scenes.active, start_ns: now_ns, stinger, stinger_clock: None });
        }
        self.scenes.active = idx;
        self.selected = None;
    }

    /// Clicking a scene: in studio mode it loads the preview, otherwise it goes live.
    pub fn select_scene(&mut self, idx: usize, now_ns: u64) {
        if idx >= self.scenes.scenes.len() {
            return;
        }
        if self.studio_mode {
            self.preview = idx;
            self.selected = None;
        } else {
            self.switch_to(idx, now_ns);
        }
    }

    pub fn set_studio_mode(&mut self, on: bool) {
        if on && !self.studio_mode {
            // OBS starts studio mode with the live scene loaded in preview.
            self.preview = self.scenes.active;
        }
        self.studio_mode = on;
        self.selected = None;
    }

    /// Studio mode "Transition" button: send preview live, previous program becomes preview.
    pub fn take_transition(&mut self, now_ns: u64) {
        if !self.studio_mode || self.preview == self.scenes.active {
            return;
        }
        let old = self.scenes.active;
        let target = self.preview;
        self.switch_to(target, now_ns);
        self.preview = old;
    }

    /// Start decoding the stinger clip; on any problem report it and fall back to a fade.
    fn open_stinger(&mut self) -> Option<StingerStream> {
        let path = self.stinger_path.trim().to_string();
        let problem = if path.is_empty() {
            Some("Choose a stinger video file in Scene Transitions (using a fade for now)".to_string())
        } else if !std::path::Path::new(&path).exists() {
            Some(format!("Stinger file not found: {path} (using a fade)"))
        } else if !ffmpeg_available() {
            Some("Stingers need ffmpeg in PATH (using a fade)".to_string())
        } else {
            None
        };
        if let Some(p) = problem {
            self.last_error = Some(p);
            return None;
        }
        let (w, h) = self.size();
        match StingerStream::open(&path, w, h, self.fps.max(1)) {
            Ok(s) => Some(s),
            Err(e) => {
                self.last_error = Some(format!("Couldn't start the stinger: {e} (using a fade)"));
                None
            }
        }
    }

    pub fn is_transitioning(&self) -> bool { self.transition.is_some() }

    /// Add an item to the scene being edited and select it.
    pub fn add_item(&mut self, item: SceneItem) -> usize {
        let scene = self.edit_scene_mut();
        scene.items.push(item);
        let idx = scene.items.len() - 1;
        self.selected = Some(idx);
        idx
    }

    pub fn remove_item(&mut self, idx: usize) {
        let scene = self.edit_scene_mut();
        if idx < scene.items.len() {
            scene.items.remove(idx);
        }
        self.selected = None;
    }

    /// Moves an item in z-order (+1 = toward front, -1 = toward back).
    pub fn move_item(&mut self, idx: usize, dir: i32) {
        let len = self.edit_scene().items.len();
        let new = idx as i32 + dir;
        if idx >= len || new < 0 || new as usize >= len {
            return;
        }
        self.edit_scene_mut().items.swap(idx, new as usize);
        if self.selected == Some(idx) {
            self.selected = Some(new as usize);
        }
    }

    /// Scale and center an item so it fits inside the canvas.
    pub fn fit_to_screen(&mut self, idx: usize) {
        let (cw, ch) = self.size();
        if let Some(item) = self.edit_scene_mut().items.get_mut(idx) {
            let (w, h) = item.cropped_size();
            let s = (cw as f32 / w as f32).min(ch as f32 / h as f32);
            item.scale = s;
            item.x = ((cw as f32 - w as f32 * s) / 2.0) as i32;
            item.y = ((ch as f32 - h as f32 * s) / 2.0) as i32;
        }
    }

    /// Bounding box of an item in canvas pixels: (x, y, w, h).
    pub fn item_bounds(&self, idx: usize) -> Option<(f32, f32, f32, f32)> {
        let item = self.edit_scene().items.get(idx)?;
        let (w, h) = item.cropped_size();
        Some((item.x as f32, item.y as f32, w as f32 * item.scale, h as f32 * item.scale))
    }

    /// Topmost item under a canvas-space point in the edited scene.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        let n = self.edit_scene().items.len();
        (0..n).rev().find(|&i| {
            let item = &self.edit_scene().items[i];
            if !item.visible { return false; }
            let (bx, by, bw, bh) = self.item_bounds(i).unwrap();
            x >= bx && x <= bx + bw && y >= by && y <= by + bh
        })
    }

    /// Tell every source whether it is part of the broadcast right now (program scene, or the
    /// outgoing scene during a transition, and visible). Media audio only plays while live.
    fn update_live_flags(&mut self) {
        let active = self.scenes.active;
        let from = self.transition.as_ref().map(|t| t.from);
        for (i, scene) in self.scenes.scenes.iter_mut().enumerate() {
            let live = i == active || Some(i) == from;
            for item in &mut scene.items {
                item.source.set_live(live && item.visible);
            }
        }
    }

    /// Render the program (live) output at time `t_ns`, handling any running transition.
    pub fn render_program(&mut self, t_ns: u64) -> Frame {
        self.update_live_flags();
        if let Some(mut tr) = self.transition.take() {
            let elapsed = t_ns.saturating_sub(tr.start_ns);
            if let Some(st) = tr.stinger.as_mut() {
                // Wait for the first decoded frame, then run the stinger on its own clock.
                if tr.stinger_clock.is_none() {
                    st.frame_at(0);
                    if st.frames_decoded() > 0 {
                        tr.stinger_clock = Some(t_ns);
                    }
                }
                let since = tr.stinger_clock.map(|c| t_ns.saturating_sub(c));
                let cut_ns = self.stinger_cut_ms as u64 * 1_000_000;
                let scene = if since.map_or(true, |s| s < cut_ns) { tr.from } else { self.scenes.active };
                let mut base = self.comp.render(&mut self.scenes.scenes[scene], t_ns);
                if let Some(s) = since {
                    let idx = s * self.fps.max(1) as u64 / 1_000_000_000;
                    if let Some(f) = st.frame_at(idx) {
                        transition::overlay(&mut base, f);
                    }
                }
                let stuck = since.is_none() && elapsed > 5_000_000_000;
                if st.finished() || stuck {
                    if st.frames_decoded() == 0 {
                        self.last_error = Some("The stinger video couldn't be played".into());
                    }
                    // Transition over; the new scene is already active.
                } else {
                    self.transition = Some(tr);
                }
                return base;
            }
            let p = elapsed as f32 / self.transition_ns.max(1) as f32;
            if p < 1.0 {
                let (from, to) = (tr.from, self.scenes.active);
                self.transition = Some(tr);
                let fa = self.comp.render(&mut self.scenes.scenes[from], t_ns);
                let fb = self.comp.render(&mut self.scenes.scenes[to], t_ns);
                return transition::apply(self.transition_kind, &fa, &fb, p);
            }
        }
        let i = self.scenes.active;
        self.comp.render(&mut self.scenes.scenes[i], t_ns)
    }

    /// Render the preview scene (studio mode) without transitions.
    pub fn render_preview(&mut self, t_ns: u64) -> Frame {
        let i = self.edit_index();
        self.comp.render(&mut self.scenes.scenes[i], t_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(c: [u8; 4]) -> SceneItem {
        SceneItem::new(Box::new(ColorSource::new("c", 8, 8, c)))
    }

    #[test]
    fn cut_switches_instantly_and_slide_moves() {
        let mut s = Studio::new(8, 8);
        s.add_item(color([255, 0, 0, 255]));
        let b = s.add_scene("B");
        s.scenes.active = b;
        s.add_item(color([0, 0, 255, 255]));
        s.scenes.active = 0;
        s.transition_kind = TransitionKind::Cut;
        s.switch_to(b, 0);
        assert!(!s.is_transitioning());
        assert_eq!(s.render_program(1).pixel(0, 0), [0, 0, 255, 255]);

        s.scenes.active = 0;
        s.transition_kind = TransitionKind::Wipe;
        s.switch_to(b, 0);
        let mid = s.render_program(150_000_000); // 50%: left half is the new scene
        assert_eq!(mid.pixel(0, 0), [0, 0, 255, 255]);
        assert_eq!(mid.pixel(7, 0), [255, 0, 0, 255]);
    }

    fn make_clip(path: &std::path::Path) -> bool {
        if !ffmpeg_available() { return false; }
        // 1 s, 10 fps, fully transparent video: it plays, but doesn't hide the scenes underneath.
        std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black@0.0:s=64x36:r=10:d=1,format=rgba", "-c:v", "qtrle"])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn stinger_switches_scene_at_the_cut_point_and_then_ends() {
        let dir = std::env::temp_dir().join(format!("rustin-studio-stinger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("s.mov");
        if !make_clip(&clip) { eprintln!("ffmpeg missing: skipped"); return; }

        let mut s = Studio::new(64, 36);
        s.fps = 10;
        s.add_item(SceneItem::new(Box::new(ColorSource::new("g", 64, 36, [0, 255, 0, 255]))));
        let b = s.add_scene("B");
        s.scenes.active = b;
        s.add_item(SceneItem::new(Box::new(ColorSource::new("b", 64, 36, [0, 0, 255, 255]))));
        s.scenes.active = 0;
        s.transition_kind = TransitionKind::Stinger;
        s.stinger_path = clip.to_string_lossy().into_owned();
        s.stinger_cut_ms = 300;
        s.switch_to(b, 0);
        assert!(s.is_transitioning() && s.last_error.is_none());

        let (mut saw_green, mut saw_blue) = (false, false);
        let mut t = 0u64;
        for _ in 0..600 {
            t += 25_000_000;
            std::thread::sleep(std::time::Duration::from_millis(5));
            let px = s.render_program(t).pixel(0, 0);
            if px == [0, 255, 0, 255] { assert!(!saw_blue, "went back to the old scene"); saw_green = true; }
            if px == [0, 0, 255, 255] { saw_blue = true; }
            if !s.is_transitioning() { break; }
        }
        assert!(saw_green && saw_blue, "green {saw_green} blue {saw_blue}");
        assert!(!s.is_transitioning(), "stinger should end when the clip ends");
        assert_eq!(s.render_program(t + 1).pixel(0, 0), [0, 0, 255, 255]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_stinger_falls_back_to_fade_with_an_error() {
        let mut s = Studio::new(8, 8);
        s.add_item(color([255, 0, 0, 255]));
        let b = s.add_scene("B");
        s.scenes.active = b;
        s.add_item(color([0, 0, 255, 255]));
        s.scenes.active = 0;
        s.transition_kind = TransitionKind::Stinger;
        s.stinger_path = "/no/such/stinger.mov".into();
        s.switch_to(b, 0);
        assert!(s.last_error.as_deref().unwrap_or("").contains("not found"));
        assert!(s.is_transitioning());
        let mid = s.render_program(150_000_000); // 50% of the default 300 ms fade
        assert_eq!(mid.pixel(0, 0), [128, 0, 128, 255]);
    }

    #[test]
    fn crop_changes_bounds_and_fit() {
        let mut s = Studio::new(100, 100);
        let i = s.add_item(color([1, 2, 3, 255])); // 8x8
        s.edit_scene_mut().items[i].crop = [2, 0, 2, 0]; // 4x8 left
        let (_, _, w, h) = s.item_bounds(i).unwrap();
        assert_eq!((w, h), (4.0, 8.0));
        s.fit_to_screen(i);
        let (_, _, w, h) = s.item_bounds(i).unwrap();
        assert_eq!((w, h), (50.0, 100.0));
    }

    #[test]
    fn srt_latency_is_converted_to_microseconds() {
        assert_eq!(stream_target("srt://h:9000", "k", 200), "srt://h:9000?streamid=k&latency=200000");
        assert_eq!(stream_target("srt://h:9000", "", 350), "srt://h:9000?latency=350000");
        assert_eq!(stream_target("srt://h:9000?mode=caller", "k", 120), "srt://h:9000?mode=caller&streamid=k&latency=120000");
        // A latency typed into the URL wins (and is passed through untouched).
        assert_eq!(stream_target("srt://h:9000?latency=500000", "k", 200), "srt://h:9000?latency=500000&streamid=k");
        // RTMP is unaffected.
        assert_eq!(stream_target("rtmp://live.twitch.tv/app", "key", 200), "rtmp://live.twitch.tv/app/key");
    }

    #[test]
    fn builds_stream_url() {
        assert_eq!(stream_url("rtmp://live.twitch.tv/app/", " abc "), "rtmp://live.twitch.tv/app/abc");
        assert_eq!(stream_url("rtmp://host/live/full-url", ""), "rtmp://host/live/full-url");
        assert_eq!(stream_url("srt://ingest.example:9000", "abc"), "srt://ingest.example:9000?streamid=abc");
        assert_eq!(stream_url("srt://h:1?mode=caller", "k"), "srt://h:1?mode=caller&streamid=k");
        assert_eq!(stream_url("srt://h:1?latency=200", ""), "srt://h:1?latency=200");
    }

    #[test]
    fn starts_with_one_empty_scene() {
        let s = Studio::new(64, 36);
        assert_eq!(s.scenes.scenes.len(), 1);
        assert!(s.active_scene().items.is_empty());
    }

    #[test]
    fn cannot_remove_last_scene() {
        let mut s = Studio::new(32, 18);
        assert!(!s.remove_scene(0));
    }

    #[test]
    fn remove_scene_fixes_indices() {
        let mut s = Studio::demo(32, 18);
        s.scenes.active = 1;
        s.preview = 1;
        assert!(s.remove_scene(0));
        assert_eq!((s.scenes.active, s.preview), (0, 0));
    }

    #[test]
    fn move_item_tracks_selection() {
        let mut s = Studio::new(32, 18);
        s.add_item(color([1, 1, 1, 255]));
        s.add_item(color([2, 2, 2, 255]));
        s.selected = Some(0);
        s.move_item(0, 1);
        assert_eq!(s.selected, Some(1));
        s.move_item(1, 1);
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn fit_to_screen_centers() {
        let mut s = Studio::new(160, 90);
        let i = s.add_item(color([9, 9, 9, 255])); // 8x8 source
        s.fit_to_screen(i);
        let (x, y, w, h) = s.item_bounds(i).unwrap();
        assert_eq!((w, h), (90.0, 90.0));
        assert_eq!((x, y), (35.0, 0.0));
    }

    #[test]
    fn hit_test_returns_topmost() {
        let mut s = Studio::new(32, 32);
        s.add_item(color([1, 0, 0, 255]));
        s.add_item(color([0, 1, 0, 255]).at(4, 4));
        assert_eq!(s.hit_test(5.0, 5.0), Some(1));
        assert_eq!(s.hit_test(1.0, 1.0), Some(0));
        assert_eq!(s.hit_test(20.0, 20.0), None);
    }

    #[test]
    fn studio_mode_edits_preview_and_transition_swaps() {
        let mut s = Studio::new(8, 8);
        s.add_item(color([255, 0, 0, 255]));
        let b = s.add_scene("B");
        s.set_studio_mode(true);
        s.select_scene(b, 0);
        s.add_item(color([0, 0, 255, 255])); // goes into B (preview)
        assert_eq!(s.scenes.scenes[b].items.len(), 1);
        assert_eq!(s.active_scene().items.len(), 1);
        s.take_transition(0);
        assert_eq!((s.scenes.active, s.preview), (b, 0));
        let mid = s.render_program(150_000_000); // half of 300 ms
        assert_eq!(mid.pixel(0, 0), [128, 0, 128, 255]);
        let end = s.render_program(400_000_000);
        assert!(!s.is_transitioning());
        assert_eq!(end.pixel(0, 0), [0, 0, 255, 255]);
    }
}
