//! System-wide hotkeys by polling the global keyboard state (works while other apps have focus).
//!
//! Windows and X11 work out of the box; macOS needs the Accessibility permission; Wayland
//! sessions don't expose global key state (hotkeys then only work through XWayland apps).

use device_query::{DeviceQuery, DeviceState, Keycode};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    ToggleRecord,
    ToggleStream,
    ToggleStudioMode,
    Transition,
    NextScene,
    PrevScene,
}

impl HotkeyAction {
    pub const ALL: [HotkeyAction; 6] = [
        HotkeyAction::ToggleRecord,
        HotkeyAction::ToggleStream,
        HotkeyAction::ToggleStudioMode,
        HotkeyAction::Transition,
        HotkeyAction::NextScene,
        HotkeyAction::PrevScene,
    ];

    /// Stable id used in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            HotkeyAction::ToggleRecord => "record",
            HotkeyAction::ToggleStream => "stream",
            HotkeyAction::ToggleStudioMode => "studio_mode",
            HotkeyAction::Transition => "transition",
            HotkeyAction::NextScene => "next_scene",
            HotkeyAction::PrevScene => "prev_scene",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HotkeyAction::ToggleRecord => "Start / stop recording",
            HotkeyAction::ToggleStream => "Start / stop streaming",
            HotkeyAction::ToggleStudioMode => "Toggle Studio Mode",
            HotkeyAction::Transition => "Transition (Studio Mode)",
            HotkeyAction::NextScene => "Next scene",
            HotkeyAction::PrevScene => "Previous scene",
        }
    }
}

/// Left/right variants of a modifier count as the same key.
fn canon(k: Keycode) -> Keycode {
    match k {
        Keycode::RControl => Keycode::LControl,
        Keycode::RShift => Keycode::LShift,
        Keycode::RAlt | Keycode::LOption | Keycode::ROption => Keycode::LAlt,
        Keycode::RCommand | Keycode::Command | Keycode::RMeta => Keycode::LMeta,
        other => other,
    }
}

fn is_modifier(k: Keycode) -> bool {
    matches!(k, Keycode::LControl | Keycode::LShift | Keycode::LAlt | Keycode::LMeta)
}

fn rank(k: Keycode) -> u8 {
    match k {
        Keycode::LControl => 0,
        Keycode::LShift => 1,
        Keycode::LAlt => 2,
        Keycode::LMeta => 3,
        _ => 4,
    }
}

/// Modifiers first, then the other keys, in a stable order.
pub fn normalize(keys: impl IntoIterator<Item = Keycode>) -> Vec<Keycode> {
    let mut v: Vec<Keycode> = keys.into_iter().map(canon).collect::<HashSet<_>>().into_iter().collect();
    v.sort_by_key(|k| (rank(*k), k.to_string()));
    v
}

/// Human-friendly text, e.g. "Ctrl+Alt+R".
pub fn display_combo(combo: &[Keycode]) -> String {
    if combo.is_empty() {
        return "(not set)".into();
    }
    combo
        .iter()
        .map(|k| match canon(*k) {
            Keycode::LControl => "Ctrl".to_string(),
            Keycode::LShift => "Shift".to_string(),
            Keycode::LAlt => "Alt".to_string(),
            Keycode::LMeta => "Meta".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Settings-file form (raw key names joined by '+').
pub fn combo_to_strings(combo: &[Keycode]) -> Vec<String> {
    combo.iter().map(|k| k.to_string()).collect()
}

pub fn combo_from_strings(names: &[String]) -> Vec<Keycode> {
    normalize(names.iter().filter_map(|n| Keycode::from_str(n).ok()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Triggered(HotkeyAction),
    /// A new combination was recorded after `begin_capture`.
    Captured(Vec<Keycode>),
}

/// Matching state shared between the sampling thread and the UI.
struct Inner {
    bindings: HashMap<HotkeyAction, Vec<Keycode>>,
    prev: HashSet<Keycode>,
    capture: Option<HashSet<Keycode>>,
    events: Vec<HotkeyEvent>,
}

impl Inner {
    fn new() -> Self {
        Self { bindings: HashMap::new(), prev: HashSet::new(), capture: None, events: Vec::new() }
    }

    /// Core logic, separate from the OS so it can be tested.
    fn step(&mut self, pressed: &HashSet<Keycode>) -> Vec<HotkeyEvent> {
        let mut events = Vec::new();
        if let Some(cap) = &mut self.capture {
            cap.extend(pressed.iter().copied());
            if pressed.is_empty() && !cap.is_empty() {
                let combo = normalize(cap.drain());
                self.capture = None;
                // A lone modifier isn't a useful hotkey.
                if combo.iter().any(|k| !is_modifier(*k)) {
                    events.push(HotkeyEvent::Captured(combo));
                }
            }
            self.prev = pressed.clone();
            return events;
        }
        for (&action, combo) in &self.bindings {
            let now = combo.iter().all(|k| pressed.contains(k));
            let before = combo.iter().all(|k| self.prev.contains(k));
            if now && !before {
                events.push(HotkeyEvent::Triggered(action));
            }
        }
        self.prev = pressed.clone();
        events
    }
}

/// Samples the keyboard on its own thread (100 Hz) so quick taps are never missed, even when
/// the UI is busy; the UI drains the resulting events with `poll`.
pub struct HotkeyPoller {
    inner: Arc<Mutex<Inner>>,
    available: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl HotkeyPoller {
    /// Connects to the OS keyboard state; if that isn't possible, hotkeys are simply inactive.
    pub fn new() -> Self {
        let inner = Arc::new(Mutex::new(Inner::new()));
        let available = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (i, a, st) = (inner.clone(), available.clone(), stop.clone());
        thread::spawn(move || {
            // DeviceState is created on this thread (it isn't guaranteed to be Send).
            let Ok(device) = std::panic::catch_unwind(DeviceState::new) else { return };
            a.store(true, Ordering::Relaxed);
            while !st.load(Ordering::Relaxed) {
                let pressed: HashSet<Keycode> = device.get_keys().into_iter().map(canon).collect();
                {
                    let mut g = i.lock().unwrap();
                    let ev = g.step(&pressed);
                    g.events.extend(ev);
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self { inner, available, stop }
    }

    pub fn available(&self) -> bool { self.available.load(Ordering::Relaxed) }

    pub fn set_binding(&mut self, action: HotkeyAction, combo: Vec<Keycode>) {
        let mut g = self.inner.lock().unwrap();
        if combo.is_empty() {
            g.bindings.remove(&action);
        } else {
            g.bindings.insert(action, normalize(combo));
        }
    }

    pub fn binding(&self, action: HotkeyAction) -> Option<Vec<Keycode>> {
        self.inner.lock().unwrap().bindings.get(&action).cloned()
    }
    pub fn is_bound(&self, action: HotkeyAction) -> bool {
        self.inner.lock().unwrap().bindings.contains_key(&action)
    }

    /// Load all bindings from the settings map (action id -> key names).
    pub fn load(&mut self, map: &BTreeMap<String, Vec<String>>) {
        for a in HotkeyAction::ALL {
            if let Some(names) = map.get(a.id()) {
                self.set_binding(a, combo_from_strings(names));
            }
        }
    }

    /// Start recording the next key combination pressed (reported once all keys are released).
    pub fn begin_capture(&mut self) { self.inner.lock().unwrap().capture = Some(HashSet::new()); }
    pub fn cancel_capture(&mut self) { self.inner.lock().unwrap().capture = None; }
    pub fn capturing(&self) -> bool { self.inner.lock().unwrap().capture.is_some() }

    /// Events gathered since the last call.
    pub fn poll(&mut self) -> Vec<HotkeyEvent> {
        std::mem::take(&mut self.inner.lock().unwrap().events)
    }
}

impl Drop for HotkeyPoller {
    fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); }
}

impl Default for HotkeyPoller {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(keys: &[Keycode]) -> HashSet<Keycode> { keys.iter().copied().collect() }
    fn poller() -> Inner { Inner::new() }
    impl Inner {
        fn set_binding(&mut self, a: HotkeyAction, combo: Vec<Keycode>) { self.bindings.insert(a, normalize(combo)); }
        fn begin_capture(&mut self) { self.capture = Some(HashSet::new()); }
    }

    #[test]
    fn combo_fires_once_when_completed() {
        let mut p = poller();
        p.set_binding(HotkeyAction::ToggleRecord, vec![Keycode::LControl, Keycode::LAlt, Keycode::R]);
        assert!(p.step(&set(&[Keycode::LControl])).is_empty());
        assert!(p.step(&set(&[Keycode::LControl, Keycode::LAlt])).is_empty());
        let ev = p.step(&set(&[Keycode::LControl, Keycode::LAlt, Keycode::R]));
        assert_eq!(ev, vec![HotkeyEvent::Triggered(HotkeyAction::ToggleRecord)]);
        // Holding the keys doesn't repeat the trigger.
        assert!(p.step(&set(&[Keycode::LControl, Keycode::LAlt, Keycode::R])).is_empty());
        // Release and press again: fires again.
        p.step(&set(&[]));
        assert_eq!(p.step(&set(&[Keycode::LControl, Keycode::LAlt, Keycode::R])).len(), 1);
    }

    #[test]
    fn right_hand_modifiers_match_left_bindings() {
        let mut p = poller();
        p.set_binding(HotkeyAction::Transition, vec![Keycode::RControl, Keycode::F5]); // stored canonical
        let pressed: HashSet<Keycode> = [Keycode::RControl, Keycode::F5].into_iter().map(canon).collect();
        assert_eq!(p.step(&pressed), vec![HotkeyEvent::Triggered(HotkeyAction::Transition)]);
    }

    #[test]
    fn capture_records_the_whole_combo_on_release() {
        let mut p = poller();
        p.begin_capture();
        assert!(p.step(&set(&[Keycode::LShift])).is_empty());
        assert!(p.step(&set(&[Keycode::LShift, Keycode::F8])).is_empty());
        assert!(p.step(&set(&[Keycode::F8])).is_empty()); // partial release keeps recording
        let ev = p.step(&set(&[]));
        assert_eq!(ev, vec![HotkeyEvent::Captured(vec![Keycode::LShift, Keycode::F8])]);
        assert!(p.capture.is_none());
    }

    #[test]
    fn modifier_only_capture_is_rejected() {
        let mut p = poller();
        p.begin_capture();
        p.step(&set(&[Keycode::LControl]));
        assert!(p.step(&set(&[])).is_empty());
    }

    #[test]
    fn no_triggers_while_capturing() {
        let mut p = poller();
        p.set_binding(HotkeyAction::ToggleStream, vec![Keycode::F10]);
        p.begin_capture();
        assert!(p.step(&set(&[Keycode::F10])).is_empty());
    }

    #[test]
    fn settings_round_trip_and_display() {
        let combo = normalize([Keycode::R, Keycode::LAlt, Keycode::RControl]);
        assert_eq!(display_combo(&combo), "Ctrl+Alt+R");
        let names = combo_to_strings(&combo);
        assert_eq!(combo_from_strings(&names), combo);
    }

    #[test]
    fn poller_loads_and_clears_bindings() {
        let combo = normalize([Keycode::LControl, Keycode::R]);
        let mut map = BTreeMap::new();
        map.insert("record".to_string(), combo_to_strings(&combo));
        let mut p = HotkeyPoller::new();
        p.load(&map);
        assert_eq!(p.binding(HotkeyAction::ToggleRecord), Some(combo));
        assert!(!p.is_bound(HotkeyAction::NextScene));
        p.set_binding(HotkeyAction::ToggleRecord, vec![]);
        assert!(!p.is_bound(HotkeyAction::ToggleRecord));
    }
}
