//! File-browser logic (listing, navigation, quick places) and dropped-file classification.

use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg"];
pub const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "m4v", "flv", "gif"];
pub const FONT_EXTS: &[&str] = &["ttf", "otf"];

fn ext_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// List `dir`: folders first, then files, each alphabetical (case-insensitive); hidden entries
/// are skipped. Files are filtered by extension (empty `exts` = all); `dirs_only` hides files.
pub fn list_dir(dir: &Path, exts: &[&str], dirs_only: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let is_dir = path.is_dir();
        if !is_dir && (dirs_only || (!exts.is_empty() && !exts.contains(&ext_of(&path).as_str()))) {
            continue;
        }
        out.push(Entry { name, path, is_dir });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

/// Shortcuts shown beside the file list: Home, common folders, and drives / root.
pub fn quick_places() -> Vec<(String, PathBuf)> {
    let mut places = Vec::new();
    if let Some(home) = home_dir() {
        for (label, sub) in [("Videos", "Videos"), ("Pictures", "Pictures"), ("Desktop", "Desktop"), ("Downloads", "Downloads"), ("Documents", "Documents")] {
            let p = home.join(sub);
            if p.is_dir() {
                places.push((label.to_string(), p));
            }
        }
        places.insert(0, ("Home".to_string(), home));
    }
    if cfg!(windows) {
        for letter in 'A'..='Z' {
            let root = PathBuf::from(format!("{letter}:\\"));
            if root.exists() {
                places.push((format!("{letter}:"), root));
            }
        }
    } else {
        places.push(("/".to_string(), PathBuf::from("/")));
    }
    places
}

pub struct Browser {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub exts: Vec<String>,
    pub folder_mode: bool,
    pub selected: Option<PathBuf>,
    pub path_text: String,
    pub error: Option<String>,
    pub places: Vec<(String, PathBuf)>,
}

impl Browser {
    /// `start` may be a file or folder (or empty); the browser opens in the nearest existing folder.
    pub fn new(start: &str, exts: &[&str], folder_mode: bool) -> Self {
        let p = PathBuf::from(start.trim());
        let dir = [Some(p.clone()), p.parent().map(Path::to_path_buf), home_dir()]
            .into_iter()
            .flatten()
            .find(|d| !d.as_os_str().is_empty() && d.is_dir())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut b = Self {
            dir: dir.clone(),
            entries: Vec::new(),
            exts: exts.iter().map(|e| e.to_string()).collect(),
            folder_mode,
            selected: None,
            path_text: String::new(),
            error: None,
            places: quick_places(),
        };
        b.navigate(dir);
        b
    }

    pub fn navigate(&mut self, dir: PathBuf) {
        let exts: Vec<&str> = self.exts.iter().map(String::as_str).collect();
        match list_dir(&dir, &exts, self.folder_mode) {
            Ok(entries) => {
                self.entries = entries;
                self.path_text = dir.to_string_lossy().into_owned();
                self.dir = dir;
                self.selected = None;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Can't open {}: {e}", dir.display())),
        }
    }

    pub fn up(&mut self) {
        if let Some(p) = self.dir.parent() {
            let p = p.to_path_buf();
            self.navigate(p);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropKind {
    Image,
    Video,
    Font,
    Other,
}

/// What to do with a file dropped onto the window.
pub fn classify(path: &Path) -> DropKind {
    let e = ext_of(path);
    if IMAGE_EXTS.contains(&e.as_str()) {
        DropKind::Image
    } else if VIDEO_EXTS.contains(&e.as_str()) {
        DropKind::Video
    } else if FONT_EXTS.contains(&e.as_str()) {
        DropKind::Font
    } else {
        DropKind::Other
    }
}

/// File name without extension, for naming a source after a dropped file.
pub fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Source".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rustin-browse-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_folders_first_filters_and_hides_dotfiles() {
        let d = tmp("list");
        fs::create_dir(d.join("zeta")).unwrap();
        fs::create_dir(d.join("Alpha")).unwrap();
        fs::create_dir(d.join(".hidden")).unwrap();
        for f in ["b.PNG", "a.jpg", "notes.txt", ".secret.png"] {
            fs::write(d.join(f), b"x").unwrap();
        }
        let names = |v: Vec<Entry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(list_dir(&d, IMAGE_EXTS, false).unwrap()), vec!["Alpha", "zeta", "a.jpg", "b.PNG"]);
        assert_eq!(names(list_dir(&d, &[], false).unwrap()), vec!["Alpha", "zeta", "a.jpg", "b.PNG", "notes.txt"]);
        assert_eq!(names(list_dir(&d, &[], true).unwrap()), vec!["Alpha", "zeta"]);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn browser_navigates_up_and_down_and_survives_bad_paths() {
        let d = tmp("nav");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub").join("clip.mp4"), b"x").unwrap();
        let mut b = Browser::new(d.join("sub").join("clip.mp4").to_str().unwrap(), VIDEO_EXTS, false);
        assert_eq!(b.dir, d.join("sub")); // opened in the file's folder
        assert_eq!(b.entries.len(), 1);
        b.up();
        assert_eq!(b.dir, d);
        assert_eq!(b.entries[0].name, "sub");
        b.navigate(d.join("does-not-exist"));
        assert!(b.error.is_some());
        assert_eq!(b.dir, d); // stayed put
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn classifies_dropped_files() {
        assert_eq!(classify(Path::new("/x/Logo.PNG")), DropKind::Image);
        assert_eq!(classify(Path::new("intro.webm")), DropKind::Video);
        assert_eq!(classify(Path::new("C:\\Fonts\\arial.ttf")), DropKind::Font);
        assert_eq!(classify(Path::new("notes.txt")), DropKind::Other);
        assert_eq!(classify(Path::new("noext")), DropKind::Other);
        assert_eq!(stem(Path::new("/a/b/My Clip.mp4")), "My Clip");
    }
}
