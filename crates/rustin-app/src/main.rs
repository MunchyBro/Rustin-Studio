use rustin_core::{
    filter::{Brightness, Grayscale},
    source::{BouncingBoxSource, ColorSource, TestPatternSource},
    transition::crossfade,
    Compositor, Scene, SceneCollection, SceneItem,
};
use rustin_output::{save_ppm, Output, Y4mOutput};
use std::{env, process};

struct Args {
    cmd: String,
    out: String,
    seconds: u32,
    fps: u32,
    width: u32,
    height: u32,
}

fn usage() -> ! {
    eprintln!(
        "Rustin Studio 0.1\n\n\
         USAGE:\n  rustin-studio record   [--out FILE.y4m] [--seconds N] [--fps N] [--width W] [--height H]\n  \
         rustin-studio snapshot [--out FILE.ppm] [--width W] [--height H]\n\n\
         The demo has two scenes (\"Main\" and \"BRB\") and cross-fades between them\n\
         halfway through the recording."
    );
    process::exit(2);
}

fn parse() -> Args {
    let mut it = env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args { cmd, out: String::new(), seconds: 6, fps: 30, width: 1280, height: 720 };
    while let Some(flag) = it.next() {
        let val = it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--out" => a.out = val,
            "--seconds" => a.seconds = val.parse().unwrap_or_else(|_| usage()),
            "--fps" => a.fps = val.parse().unwrap_or_else(|_| usage()),
            "--width" => a.width = val.parse().unwrap_or_else(|_| usage()),
            "--height" => a.height = val.parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    if a.out.is_empty() {
        a.out = if a.cmd == "snapshot" { "snapshot.ppm".into() } else { "recording.y4m".into() };
    }
    a
}

fn demo_collection(w: u32, h: u32) -> SceneCollection {
    let mut main = Scene::new("Main");
    main.add(SceneItem::new(Box::new(TestPatternSource::new("pattern", w, h))));
    main.add(
        SceneItem::new(Box::new(BouncingBoxSource::new("logo", w / 2, h / 2, h / 10, [255, 220, 0, 255])))
            .at(w as i32 / 4, h as i32 / 4)
            .with_opacity(0.85),
    );
    main.add(
        SceneItem::new(Box::new(TestPatternSource::new("cam", w / 4, h / 4)))
            .at((w - w / 4 - 20) as i32, (h - h / 4 - 20) as i32)
            .with_filter(Box::new(Grayscale)),
    );

    let mut brb = Scene::new("BRB");
    brb.add(SceneItem::new(Box::new(ColorSource::new("bg", w, h, [30, 30, 60, 255]))));
    brb.add(
        SceneItem::new(Box::new(TestPatternSource::new("badge", w / 3, h / 3)))
            .at((w / 3) as i32, (h / 3) as i32)
            .with_filter(Box::new(Brightness(0.7))),
    );

    let mut c = SceneCollection::default();
    c.add_scene(main);
    c.add_scene(brb);
    c
}

fn main() {
    let a = parse();
    let mut comp = Compositor::new(a.width, a.height);
    let mut scenes = demo_collection(a.width, a.height);

    match a.cmd.as_str() {
        "snapshot" => {
            let idx = scenes.active;
            let frame = comp.render(&mut scenes.scenes[idx], 1_500_000_000);
            save_ppm(&frame, &a.out).expect("write snapshot");
            println!("wrote {}", a.out);
        }
        "record" => {
            let mut out = Y4mOutput::create(&a.out).expect("create output");
            out.start(a.width, a.height, a.fps).unwrap();

            let total = a.seconds * a.fps;
            let switch_at = total / 2;
            let fade_frames = (a.fps / 2).max(1); // 0.5 s transition
            let (from, to) = (scenes.find("Main").unwrap(), scenes.find("BRB").unwrap());

            for n in 0..total {
                let t_ns = n as u64 * 1_000_000_000 / a.fps as u64;
                let frame = if n >= switch_at && n < switch_at + fade_frames {
                    let t = (n - switch_at) as f32 / fade_frames as f32;
                    let fa = comp.render(&mut scenes.scenes[from], t_ns);
                    let fb = comp.render(&mut scenes.scenes[to], t_ns);
                    crossfade(&fa, &fb, t)
                } else {
                    let idx = if n < switch_at { from } else { to };
                    comp.render(&mut scenes.scenes[idx], t_ns)
                };
                out.push_video(&frame).unwrap();
            }
            out.stop().unwrap();
            println!("recorded {total} frames ({}x{} @ {} fps) -> {}", a.width, a.height, a.fps, a.out);
        }
        _ => usage(),
    }
}
