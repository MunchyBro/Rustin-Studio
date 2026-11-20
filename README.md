<p align="center">
  <img src="assets/rustin-studio-logo.png" alt="Rustin Studio logo" width="180" />
</p>

# Rustin Studio

An OBS-style screen recorder and scene compositor written in Rust. GPL-2.0-or-later.

    cargo run --release -p rustin-gui              # empty scene, like a fresh OBS
    cargo run --release -p rustin-gui -- --demo    # two sample scenes
    cargo run --release -p rustin-gui -- --display # starts with your primary display captured

## What works
- **Sources:** Display Capture, Window Capture, Image (PNG/JPEG), **Media (video files, looping, with sound)**, Text, Color, Test Pattern
- **File browser** built into the app (Browse... buttons for images, videos, stingers, fonts and the
  recording folder) and **drag & drop**: drop an image or video on the window to add it as a source,
  or a .ttf/.otf onto a selected text source. Video playback needs `ffmpeg`/`ffprobe` in PATH.
  A video's soundtrack gets its own channel in the Audio Mixer (meter, fader, mute) and goes into
  recordings and streams, but only while that source is in the live scene and visible.
  Every mixer channel has a **Monitor** box that also plays it through your speakers (on by default for
  video, off for microphones to avoid feedback). With Windows' Desktop Audio capture active, a monitored
  video would be captured a second time, so monitoring then starts off; tick it only if you know you want that.
- **Text sources:** multi-line, size, color, background, optional custom .ttf/.otf font
  (bundled: Ubuntu Light under the Ubuntu Font Licence, see `crates/rustin-core/assets/UFL.txt`)
- **Crop** any edge of a source, and an ordered, switchable **filter list** per source (Properties):
  Chroma Key (key color, similarity, smoothness, spill reduction), Blur, Color Correction
  (brightness, contrast, saturation, gamma), Sharpen, **Color LUT** (3D `.cube` files), **Image Mask**
  (alpha / luma / inverted luma), **Scroll** (endless wrap-around ticker, either direction) and **Flip**.
  Blur and LUT are CPU-heavy on very large sources.
- **Preview canvas:** click to select, drag to move, drag corner handles to scale,
  arrow keys nudge (Shift = 10px), Delete removes
- **Scenes** with Cut, Fade, Slide (either direction), Wipe and **Stinger** (video) transitions, and **Studio Mode** (Preview / Program + Transition button)
- **Audio Mixer:** live level meters, volume, mute (mic everywhere; desktop audio via loopback on Windows)
- **Streaming:** RTMP (Twitch, YouTube or custom server) and **SRT** (`srt://host:port`; the key is sent as the SRT streamid and Settings has an **SRT latency (ms)** box, converted for you because ffmpeg's own `latency` option is in microseconds) with H.264 + AAC, live mic/desktop audio,
  configurable bitrate, LIVE timer and dropped-frame counter. Needs `ffmpeg` in PATH.
  Set the server and stream key in Settings (the key is kept in memory only, never saved).
  Try it locally: `ffmpeg -listen 1 -f flv -i rtmp://127.0.0.1:1935/live/test -c copy out.flv`,
  then `cargo run --release -p rustin-gui -- --stream-url rtmp://127.0.0.1:1935/live/test`.
- **Recording and streaming stay in sync:** frames are stamped with the real clock and ffmpeg holds a constant frame rate
  (repeating a frame if rendering runs late), so video never drifts from audio even on a loaded machine.
  Both outputs write on background threads, so a slow disk or network drops frames instead of freezing the UI.
- **Encoding (Settings > Encoding):** pick the video encoder: x264 (CPU), **NVIDIA NVENC, Intel Quick Sync,
  AMD AMF or Apple VideoToolbox**. At startup each one is *test-encoded* in the background and only those
  that really work can be selected; if the chosen one isn't usable the app falls back to x264 and says so.
  Also: recording quality (CRF-style slider) and recording format **MP4 or MKV**. The hardware encoders'
  settings are written from ffmpeg's documentation and checked by the probe, but have not been run on
  real GPUs by the author: report any that misbehave.
- **Recording:** H.264 through `ffmpeg` (must be in PATH). While recording, the file is crash-safe
  (fragmented MP4 / MKV, so a crash or power cut still leaves a playable file); when you stop, it is
  remuxed with the audio tracks into the final MP4/MKV. Without ffmpeg it falls back to raw `.y4m`.
- **Settings:** canvas resolution (1080p / 720p / 480p), frame rate, recording folder

## Stinger transitions
Pick *Stinger (video)* in Scene Transitions, enter the path of a video file and a cut point (ms into
the clip at which the scene actually switches). Any format ffmpeg can read works; use a clip with an
alpha channel (`.mov` with qtrle/ProRes 4444, or VP9 `.webm`) so the animation can reveal the scene
underneath, or an opaque clip that fully covers the screen at the cut point. The clip is scaled to the
canvas. If the file is missing or ffmpeg isn't available, it falls back to a fade and says why.

## Keyboard shortcuts
**System-wide hotkeys** (Settings > Hotkeys): click *Set*, press your combination, release.
Start/stop recording, start/stop streaming, toggle Studio Mode, Transition, next/previous scene.
They work while another app is focused. The keyboard is sampled on a 100 Hz background thread, so
quick taps are never missed. Windows and X11 work directly; macOS needs the Accessibility
permission; pure Wayland sessions don't expose global key state.

**In-window shortcuts:** F9 record, F10 stream, Ctrl+M studio mode, Ctrl+Enter transition,
Ctrl+1..9 select scene, Delete / arrow keys edit the selected source. An action that has a
system-wide hotkey is handled there instead, so it never fires twice. Help > Keyboard Shortcuts.

## Saved automatically
Scenes (sources, positions, scale, opacity, effects, transition length), audio fader/mute levels,
and settings are saved every few seconds and on exit, and restored on launch. Files live in
`%APPDATA%\rustin-studio` (Windows), `~/.config/rustin-studio` (Linux) or
`~/Library/Application Support/rustin-studio` (macOS). The **stream key is never written to disk**.
Display capture is restored by monitor name; window capture re-finds its window by title
(then by application) and waits for it to appear. `--demo` is ephemeral and never touches saved files.

## Performance
**GPU compositing (experimental, Settings > Video):** scenes are drawn on the graphics card through
wgpu (Vulkan / Metal / DirectX 12) and read back for the encoder. Output is bit-identical to the CPU
path in the test scene. It falls back to the CPU automatically if no adapter is usable, and says so
if it ends up on a software renderer. Filters still run on the CPU before upload, so the gain is
mostly for large canvases and many layers. It is off by default; measure on your own machine.
`cargo run --release -p rustin-gpu --example bench_gpu` compares both. (On the single-core test machine,
whose only "GPU" was a software renderer, the CPU path was faster: 13 ms vs 55 ms per 1080p frame.)

**CPU compositing (default):** the compositor runs on the CPU: sources are rendered, then the canvas is split into bands
composited in parallel (up to 8 threads), with a memcpy fast path for opaque unscaled sources.
Benchmark: `cargo run --release -p rustin-core --example bench`.
On a single-core test machine a 1080p scene with four sources composites in ~10.6 ms per frame
(~13 ms before these optimizations); more cores reduce that further. Large canvases show the
preview at half resolution to keep the UI cheap. 

## Not implemented yet
RTMPS (depends on your ffmpeg build having TLS; untested here), audio capture from individual windows or displays.

## Building

**Windows:** install [Rust](https://rustup.rs) (it will ask for the Visual Studio C++ Build Tools) and
[ffmpeg](https://ffmpeg.org/download.html) (put `ffmpeg.exe` / `ffprobe.exe` on your PATH, or next to
the app), then:

    cargo run --release -p rustin-gui

**Linux:** needs a recent stable Rust plus these packages (Debian/Ubuntu names):
`libasound2-dev libwayland-dev libdbus-1-dev libpipewire-0.3-dev libclang-dev libgbm-dev
libegl1-mesa-dev libxcb-randr0-dev libxcb-shm0-dev libx11-dev libxi-dev libxtst-dev libxkbcommon-dev
libxkbcommon-x11-dev ffmpeg`. **macOS:** `brew install ffmpeg`; screen capture and global hotkeys
need the Screen Recording and Accessibility permissions.

### GitHub
`.github/workflows/ci.yml` builds and tests on Linux, Windows and macOS for every push and pull request
(Windows and macOS are non-blocking until they have gone green once). `.github/workflows/release.yml`
packages downloads: push a tag such as `v0.8.0` and it publishes a GitHub release with a Windows zip
(app + ffmpeg, no install needed), plus Linux and macOS archives. You can also run it by hand from the
Actions tab to get the packages as build artifacts without publishing.

## Crates
`rustin-core` frames/sources/scenes/compositor/transitions - `rustin-capture` display, window, image sources -
`rustin-audio` inputs, meters, WAV recording - `rustin-hotkeys` global hotkeys - `rustin-gpu` wgpu compositor - `rustin-output` ffmpeg MP4 + y4m - `rustin-gui` the app -
`rustin-app` command-line demo
