//! Prints hotkey events for ~6 s. Binds Ctrl+Alt+R to "record" and captures one new combo.
//! cargo run -p rustin-hotkeys --example listen
use rustin_hotkeys::{normalize, HotkeyAction, HotkeyPoller};
use device_query::Keycode;
use std::{thread, time::{Duration, Instant}};

fn main() {
    let mut p = HotkeyPoller::new();
    p.set_binding(HotkeyAction::ToggleRecord, normalize([Keycode::LControl, Keycode::LAlt, Keycode::R]));
    thread::sleep(Duration::from_millis(300));
    println!("available: {}", p.available());
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(6) {
        for ev in p.poll() { println!("{:?}", ev); }
        thread::sleep(Duration::from_millis(50));
    }
}
