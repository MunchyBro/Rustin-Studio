//! Streams 5 s of animated test video + a 440 Hz tone to an RTMP URL.
//! Usage: cargo run -p rustin-output --example stream_test -- rtmp://127.0.0.1:1935/live/test
use rustin_core::{source::TestPatternSource, Compositor, Scene, SceneItem};
use rustin_output::{Output, RtmpOutput};
use std::{thread, time::{Duration, Instant}};

fn main() {
    let url = std::env::args().nth(1).expect("usage: stream_test <rtmp-url>");
    let (w, h, fps) = (640u32, 360u32, 30u32);
    let mut phase = 0f32;
    let audio = Box::new(move |frames: usize| {
        let mut v = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let s = (phase * std::f32::consts::TAU).sin() * 0.3;
            phase = (phase + 440.0 / 48_000.0).fract();
            v.push(s);
            v.push(s);
        }
        v
    });
    let mut out = RtmpOutput::new(url, 2000).with_audio(audio);
    out.start(w, h, fps).expect("start");
    let mut scene = Scene::new("t");
    scene.add(SceneItem::new(Box::new(TestPatternSource::new("p", w, h))));
    let mut comp = Compositor::new(w, h);
    let t0 = Instant::now();
    let mut n = 0u64;
    while t0.elapsed() < Duration::from_secs(5) {
        let target = (t0.elapsed().as_secs_f64() * fps as f64) as u64;
        while n <= target {
            let f = comp.render(&mut scene, n * 1_000_000_000 / fps as u64);
            out.push_video(&f).expect("push");
            n += 1;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let s = out.stats();
    println!("sent {n} frames, dropped {}, alive {}, last msg: {:?}", s.dropped_frames, s.alive, s.last_error);
    out.stop().unwrap();
}
