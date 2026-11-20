//! Video encoders: which ffmpeg H.264 encoder to use, its arguments, and a real test-encode probe.

use crate::ffmpeg_cmd;
use std::{
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoEncoder {
    /// Software (CPU). Works everywhere ffmpeg has libx264.
    X264,
    /// NVIDIA NVENC.
    Nvenc,
    /// Intel Quick Sync.
    Qsv,
    /// AMD AMF.
    Amf,
    /// Apple VideoToolbox.
    VideoToolbox,
}

impl VideoEncoder {
    pub const ALL: [VideoEncoder; 5] = [
        VideoEncoder::X264,
        VideoEncoder::Nvenc,
        VideoEncoder::Qsv,
        VideoEncoder::Amf,
        VideoEncoder::VideoToolbox,
    ];

    /// Stable id for the settings file.
    pub fn id(self) -> &'static str {
        match self {
            VideoEncoder::X264 => "x264",
            VideoEncoder::Nvenc => "nvenc",
            VideoEncoder::Qsv => "qsv",
            VideoEncoder::Amf => "amf",
            VideoEncoder::VideoToolbox => "videotoolbox",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            VideoEncoder::X264 => "x264 (CPU)",
            VideoEncoder::Nvenc => "NVIDIA NVENC",
            VideoEncoder::Qsv => "Intel Quick Sync",
            VideoEncoder::Amf => "AMD AMF",
            VideoEncoder::VideoToolbox => "Apple VideoToolbox",
        }
    }

    pub fn codec(self) -> &'static str {
        match self {
            VideoEncoder::X264 => "libx264",
            VideoEncoder::Nvenc => "h264_nvenc",
            VideoEncoder::Qsv => "h264_qsv",
            VideoEncoder::Amf => "h264_amf",
            VideoEncoder::VideoToolbox => "h264_videotoolbox",
        }
    }

    /// Constant-quality arguments for recording. `quality` is CRF-like: lower = better (14..=32).
    pub fn record_args(self, quality: u32) -> Vec<String> {
        let q = quality.clamp(10, 40);
        let s = |v: &str| v.to_string();
        let mut a = vec![s("-c:v"), s(self.codec())];
        match self {
            VideoEncoder::X264 => a.extend([s("-preset"), s("veryfast"), s("-crf"), q.to_string(), s("-pix_fmt"), s("yuv420p")]),
            VideoEncoder::Nvenc => a.extend([s("-preset"), s("p5"), s("-rc"), s("vbr"), s("-cq"), q.to_string(), s("-b:v"), s("0"), s("-pix_fmt"), s("yuv420p")]),
            VideoEncoder::Qsv => a.extend([s("-global_quality"), q.to_string(), s("-look_ahead"), s("0"), s("-pix_fmt"), s("nv12")]),
            VideoEncoder::Amf => a.extend([
                s("-rc"), s("cqp"), s("-qp_i"), q.to_string(), s("-qp_p"), q.to_string(), s("-qp_b"), q.to_string(), s("-pix_fmt"), s("yuv420p"),
            ]),
            // VideoToolbox's -q:v is 1..100, higher = better.
            VideoEncoder::VideoToolbox => a.extend([s("-q:v"), (100 - q * 2).clamp(1, 100).to_string(), s("-pix_fmt"), s("yuv420p")]),
        }
        a
    }

    /// Bitrate-controlled, low-latency arguments for live streaming.
    pub fn stream_args(self, bitrate_kbps: u32, fps: u32) -> Vec<String> {
        let k = bitrate_kbps.max(300);
        let s = |v: &str| v.to_string();
        let (b, buf, gop) = (format!("{k}k"), format!("{}k", k * 2), (fps * 2).to_string());
        let mut a = vec![s("-c:v"), s(self.codec())];
        match self {
            VideoEncoder::X264 => a.extend([s("-preset"), s("veryfast"), s("-tune"), s("zerolatency"), s("-pix_fmt"), s("yuv420p")]),
            VideoEncoder::Nvenc => a.extend([s("-preset"), s("p4"), s("-tune"), s("ll"), s("-rc"), s("cbr"), s("-pix_fmt"), s("yuv420p")]),
            VideoEncoder::Qsv => a.extend([s("-look_ahead"), s("0"), s("-pix_fmt"), s("nv12")]),
            VideoEncoder::Amf => a.extend([s("-rc"), s("cbr"), s("-usage"), s("lowlatency"), s("-pix_fmt"), s("yuv420p")]),
            VideoEncoder::VideoToolbox => a.extend([s("-realtime"), s("1"), s("-pix_fmt"), s("yuv420p")]),
        }
        a.extend([s("-b:v"), b.clone(), s("-maxrate"), b, s("-bufsize"), buf, s("-g"), gop]);
        a
    }
}

/// Really encode a few frames with this encoder (the same arguments recording will use). A GPU
/// encoder that merely appears in ffmpeg's list but can't start (no card, old driver) fails here.
pub fn encoder_works(enc: VideoEncoder) -> bool {
    let mut cmd = ffmpeg_cmd();
    cmd.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=256x144:r=30", "-frames:v", "5"])
        .args(enc.record_args(23))
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return false };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Probe every encoder (a second or two in total); do this on a background thread.
pub fn detect_encoders() -> Vec<(VideoEncoder, bool)> {
    VideoEncoder::ALL.into_iter().map(|e| (e, encoder_works(e))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg_available;

    fn has(args: &[String], pair: [&str; 2]) -> bool {
        args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }

    #[test]
    fn ids_round_trip_and_unknown_ids_are_rejected() {
        for e in VideoEncoder::ALL {
            assert_eq!(VideoEncoder::from_id(e.id()), Some(e));
        }
        assert_eq!(VideoEncoder::from_id("nope"), None);
    }

    #[test]
    fn record_args_set_the_codec_and_quality() {
        let a = VideoEncoder::X264.record_args(20);
        assert!(has(&a, ["-c:v", "libx264"]) && has(&a, ["-crf", "20"]));
        assert!(has(&VideoEncoder::Nvenc.record_args(24), ["-cq", "24"]));
        assert!(has(&VideoEncoder::Qsv.record_args(24), ["-global_quality", "24"]));
        assert!(has(&VideoEncoder::Amf.record_args(24), ["-qp_i", "24"]));
        // VideoToolbox quality is 1..100 (higher = better), so a lower CRF-like value gives a higher number.
        let hi = VideoEncoder::VideoToolbox.record_args(14);
        let lo = VideoEncoder::VideoToolbox.record_args(30);
        let q = |a: &[String]| a[a.iter().position(|x| x == "-q:v").unwrap() + 1].parse::<i32>().unwrap();
        assert!(q(&hi) > q(&lo));
        // Out-of-range values are clamped rather than passed through.
        assert!(has(&VideoEncoder::X264.record_args(999), ["-crf", "40"]));
    }

    #[test]
    fn stream_args_use_the_bitrate_and_a_two_second_gop() {
        for e in VideoEncoder::ALL {
            let a = e.stream_args(6000, 30);
            assert!(has(&a, ["-c:v", e.codec()]), "{e:?}");
            assert!(has(&a, ["-b:v", "6000k"]) && has(&a, ["-maxrate", "6000k"]) && has(&a, ["-bufsize", "12000k"]), "{e:?}");
            assert!(has(&a, ["-g", "60"]), "{e:?}");
        }
        assert!(has(&VideoEncoder::X264.stream_args(10, 30), ["-b:v", "300k"])); // sane floor
    }

    #[test]
    fn software_encoder_probes_as_working_and_missing_hardware_does_not() {
        if !ffmpeg_available() { eprintln!("ffmpeg missing: skipped"); return; }
        assert!(encoder_works(VideoEncoder::X264));
        // Not asserting a specific GPU encoder: only that the probe returns promptly either way.
        let t0 = Instant::now();
        let _ = encoder_works(VideoEncoder::VideoToolbox);
        assert!(t0.elapsed() < Duration::from_secs(12));
    }
}
