//! Saving and restoring settings and scene collections (JSON in the user's config dir).

use crate::Studio;
use rustin_capture::{probe_has_audio, AudioSink, CaptureSource, ImageSource};
use rustin_core::{
    source::{BouncingBoxSource, ColorSource, TestPatternSource},
    Effect, Scene, SceneCollection, SceneItem, Source, SourceSpec, TextSource, TransitionKind,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::PathBuf};

pub const DEFAULT_SERVER: &str = "rtmp://live.twitch.tv/app";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioPref {
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    /// Play this input through the speakers as well.
    #[serde(default)]
    pub monitor: bool,
}

/// App settings. The stream key is deliberately never serialized.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub rec_dir: String,
    pub canvas: usize,
    pub fps: usize,
    pub service: usize,
    pub server: String,
    #[serde(skip)]
    pub key: String,
    pub bitrate: u32,
    /// Video encoder id: "x264", "nvenc", "qsv", "amf" or "videotoolbox".
    pub encoder: String,
    /// Recording quality, CRF-style: lower = better and bigger (14..=32).
    pub rec_quality: u32,
    /// Recording container: "mp4" or "mkv".
    pub rec_format: String,
    /// SRT receiver delay in milliseconds; higher survives worse networks at the cost of delay.
    pub srt_latency_ms: u32,
    /// Composite scenes on the GPU instead of the CPU (falls back automatically on failure).
    pub gpu: bool,
    pub audio: Vec<AudioPref>,
    /// Global hotkeys: action id -> key names (see rustin-hotkeys).
    pub hotkeys: BTreeMap<String, Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            rec_dir: default_rec_dir(),
            canvas: 1,
            fps: 1,
            service: 0,
            server: DEFAULT_SERVER.into(),
            key: String::new(),
            bitrate: 4500,
            encoder: "x264".into(),
            rec_quality: 20,
            rec_format: "mp4".into(),
            srt_latency_ms: 200,
            gpu: false,
            audio: Vec::new(),
            hotkeys: BTreeMap::new(),
        }
    }
}

fn default_rec_dir() -> String {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(h) => PathBuf::from(h).join("Videos").join("Rustin Studio").to_string_lossy().into_owned(),
        None => "recordings".into(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemSpec {
    pub source: SourceSpec,
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    pub opacity: f32,
    pub visible: bool,
    pub grayscale: bool,
    pub brightness: f32,
    #[serde(default)]
    pub crop: [u32; 4],
    #[serde(default)]
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneSpec {
    pub name: String,
    pub items: Vec<ItemSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectionSpec {
    pub scenes: Vec<SceneSpec>,
    pub active: usize,
    pub transition_ms: u32,
    #[serde(default)]
    pub transition: TransitionKind,
    #[serde(default)]
    pub stinger_path: String,
    #[serde(default = "default_cut_ms")]
    pub stinger_cut_ms: u32,
}

fn default_cut_ms() -> u32 {
    500
}

pub fn snapshot(studio: &Studio) -> CollectionSpec {
    CollectionSpec {
        scenes: studio
            .scenes
            .scenes
            .iter()
            .map(|sc| SceneSpec {
                name: sc.name.clone(),
                items: sc
                    .items
                    .iter()
                    .map(|it| ItemSpec {
                        source: it.source.spec(),
                        x: it.x,
                        y: it.y,
                        scale: it.scale,
                        opacity: it.opacity,
                        visible: it.visible,
                        grayscale: it.grayscale,
                        brightness: it.brightness,
                        crop: it.crop,
                        effects: it.effects.clone(),
                    })
                    .collect(),
            })
            .collect(),
        active: studio.scenes.active,
        transition_ms: (studio.transition_ns / 1_000_000) as u32,
        transition: studio.transition_kind,
        stinger_path: studio.stinger_path.clone(),
        stinger_cut_ms: studio.stinger_cut_ms,
    }
}

/// `audio` creates a mixer input for sources that have sound (called only when there is some).
pub fn build_source(spec: &SourceSpec, fps: u32, audio: &mut dyn FnMut(&str) -> Option<AudioSink>) -> Result<Box<dyn Source>, String> {
    Ok(match spec {
        SourceSpec::Color { name, rgba, width, height } => Box::new(ColorSource::new(name, *width, *height, *rgba)),
        SourceSpec::TestPattern { name, width, height } => Box::new(TestPatternSource::new(name, *width, *height)),
        SourceSpec::BouncingBox { name, width, height, box_size, rgba } => {
            Box::new(BouncingBoxSource::new(name, *width, *height, *box_size, *rgba))
        }
        SourceSpec::Display { name, monitor_name, width, height } => {
            Box::new(CaptureSource::display_by_name(name, monitor_name, (*width, *height), fps))
        }
        SourceSpec::Window { name, app, title, width, height } => {
            Box::new(CaptureSource::window_matching(name, app, title, (*width, *height), fps))
        }
        SourceSpec::Text { name, text, size, rgba, background, font_path } => {
            let mut t = TextSource::new(name, text, *size, *rgba);
            t.background = *background;
            t.font_path = font_path.clone();
            t.update();
            Box::new(t)
        }
        SourceSpec::Media { name, path, looping, .. } => {
            let sink = if probe_has_audio(path) { audio(name) } else { None };
            Box::new(CaptureSource::media(name, path, *looping, sink).map_err(|e| format!("{name}: {e}"))?)
        }
        SourceSpec::Image { name, path } => Box::new(ImageSource::open(name, path).map_err(|e| format!("{name}: {e}"))?),
    })
}

/// Replace the studio's scenes with a saved collection. Returns warnings for anything skipped.
pub fn restore(studio: &mut Studio, spec: &CollectionSpec, fps: u32, audio: &mut dyn FnMut(&str) -> Option<AudioSink>) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut scenes = Vec::new();
    for ss in &spec.scenes {
        let mut scene = Scene::new(&ss.name);
        for it in &ss.items {
            match build_source(&it.source, fps, audio) {
                Ok(src) => {
                    let mut item = SceneItem::new(src);
                    item.x = it.x;
                    item.y = it.y;
                    item.scale = it.scale;
                    item.opacity = it.opacity;
                    item.visible = it.visible;
                    item.grayscale = it.grayscale;
                    item.brightness = it.brightness;
                    item.crop = it.crop;
                    item.effects = it.effects.clone();
                    scene.items.push(item);
                }
                Err(e) => warnings.push(format!("Skipped a source ({e})")),
            }
        }
        scenes.push(scene);
    }
    if scenes.is_empty() {
        return warnings;
    }
    let active = spec.active.min(scenes.len() - 1);
    studio.scenes = SceneCollection { scenes, active };
    studio.preview = active;
    studio.selected = None;
    studio.transition_ns = spec.transition_ms as u64 * 1_000_000;
    studio.transition_kind = spec.transition;
    studio.stinger_path = spec.stinger_path.clone();
    studio.stinger_cut_ms = spec.stinger_cut_ms;
    warnings
}

// ------------------------------------------------------------------ files

pub fn config_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("rustin-studio")
}

/// Load `name` from the config dir; `None` if missing or unreadable.
pub fn load_json<T: DeserializeOwned>(name: &str) -> Option<T> {
    let text = fs::read_to_string(config_dir().join(name)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write via a temp file + rename so a crash can't leave a half-written config.
pub fn save_text(name: &str, text: &str) -> io::Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Studio;

    #[test]
    fn scenes_round_trip() {
        let mut a = Studio::new(320, 180);
        a.add_item(SceneItem::new(Box::new(ColorSource::new("Blue", 320, 180, [0, 0, 255, 255]))));
        let i = a.add_item(SceneItem::new(Box::new(TestPatternSource::new("Pat", 100, 50))).at(10, 20).scaled(1.5).with_opacity(0.5));
        a.edit_scene_mut().items[i].grayscale = true;
        a.edit_scene_mut().items[i].brightness = 0.8;
        a.add_scene("Second");
        a.transition_ns = 700_000_000;
        a.transition_kind = TransitionKind::Stinger;
        a.stinger_path = "/clips/intro.mov".into();
        a.stinger_cut_ms = 750;
        a.edit_scene_mut().items[i].crop = [4, 3, 2, 1];
        a.edit_scene_mut().items[i].effects = vec![
            Effect::new(rustin_core::FilterSpec::chroma_key()),
            Effect { enabled: false, spec: rustin_core::FilterSpec::Blur { radius: 9 } },
        ];
        let mut txt = TextSource::new("Caption", "Hi\nthere", 40.0, [255, 200, 0, 255]);
        txt.background = [0, 0, 0, 120];
        txt.update();
        a.add_item(SceneItem::new(Box::new(txt)).at(5, 5));

        let json = serde_json::to_string_pretty(&snapshot(&a)).unwrap();
        let spec: CollectionSpec = serde_json::from_str(&json).unwrap();
        let mut b = Studio::new(320, 180);
        assert!(restore(&mut b, &spec, 30, &mut |_| None).is_empty());

        assert_eq!(b.scenes.scenes.len(), 2);
        assert_eq!(b.scenes.scenes[1].name, "Second");
        assert_eq!(b.transition_ns, 700_000_000);
        assert_eq!(b.transition_kind, TransitionKind::Stinger);
        assert_eq!((b.stinger_path.as_str(), b.stinger_cut_ms), ("/clips/intro.mov", 750));
        assert_eq!(b.scenes.scenes[0].items[1].crop, [4, 3, 2, 1]);
        assert_eq!(b.scenes.scenes[0].items[1].effects, a.scenes.scenes[0].items[1].effects);
        assert_eq!(b.scenes.scenes[0].items[2].source.spec(), a.scenes.scenes[0].items[2].source.spec());
        let it = &b.scenes.scenes[0].items[1];
        assert_eq!((it.x, it.y, it.scale, it.opacity, it.grayscale, it.brightness), (10, 20, 1.5, 0.5, true, 0.8));
        assert_eq!(it.source.name(), "Pat");
        // Same render result before and after.
        let fa = a.render_program(0);
        let fb = b.render_program(0);
        assert_eq!(fa.data, fb.data);
    }

    #[test]
    fn missing_image_is_skipped_with_warning() {
        let spec = CollectionSpec {
            scenes: vec![SceneSpec {
                name: "S".into(),
                items: vec![ItemSpec {
                    source: SourceSpec::Image { name: "pic".into(), path: "/nonexistent/x.png".into() },
                    x: 0, y: 0, scale: 1.0, opacity: 1.0, visible: true, grayscale: false, brightness: 1.0, crop: [0; 4], effects: vec![],
                }],
            }],
            active: 0,
            transition_ms: 300,
            transition: TransitionKind::Fade,
            stinger_path: String::new(),
            stinger_cut_ms: 500,
        };
        let mut s = Studio::new(64, 36);
        let w = restore(&mut s, &spec, 30, &mut |_| None);
        assert_eq!(w.len(), 1);
        assert!(s.scenes.scenes[0].items.is_empty());
    }

    #[test]
    fn stream_key_is_never_serialized() {
        let mut st = Settings::default();
        st.key = "live_super_secret".into();
        let json = serde_json::to_string(&st).unwrap();
        assert!(!json.contains("super_secret"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(back.key.is_empty());
    }

    #[test]
    fn hotkey_bindings_persist_in_settings() {
        let mut st = Settings::default();
        st.hotkeys.insert("record".into(), vec!["LControl".into(), "R".into()]);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back.hotkeys["record"], vec!["LControl".to_string(), "R".to_string()]);
    }

    #[test]
    fn old_settings_files_still_load() {
        let st: Settings = serde_json::from_str(r#"{"fps": 2}"#).unwrap();
        assert_eq!(st.fps, 2);
        assert!(!st.gpu);
        assert_eq!(st.srt_latency_ms, 200);
        assert_eq!((st.encoder.as_str(), st.rec_quality, st.rec_format.as_str()), ("x264", 20, "mp4"));
        assert_eq!(st.bitrate, 4500);
    }
}
