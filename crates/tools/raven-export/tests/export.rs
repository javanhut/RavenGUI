//! End to end: a recording in, an MP4 out, checked by ffprobe and ffmpeg.
//!
//! ffmpeg is the oracle only. Where it is missing these tests say so and pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// A desktop-ish RGBA frame: a gradient and a light square at (`x`, `y`).
fn frame(w: usize, h: usize, x: usize, y: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        for col in 0..w {
            let inside = (x..x + 16).contains(&col) && (y..y + 16).contains(&row);
            let pixel = if inside {
                [230, 220, 90, 255]
            } else {
                [(col * 255 / w) as u8, (row * 255 / h) as u8, 120, 255]
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    rgba
}

fn write_recording(path: &Path, w: usize, h: usize, frames: &[(u64, Vec<u8>)], end_ms: u64) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = raven_rec::Encoder::new(file, w as u32, h as u32).unwrap();
    for (ms, rgba) in frames {
        encoder.push(Duration::from_millis(*ms), rgba).unwrap();
    }
    encoder.finish(Duration::from_millis(end_ms)).unwrap();
}

fn probe(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames"])
        .args(["-show_entries", "stream=codec_name,profile,width,height,nb_read_frames,color_space,color_range:format=duration"])
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn field<'a>(probe: &'a str, key: &str) -> &'a str {
    probe
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in {probe}"))
}

#[test]
fn a_recording_exports_to_an_mp4_that_plays() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("ffmpeg is not installed; skipping");
        return;
    }
    let (w, h) = (96, 64);
    // Steady motion, then a long still: variable timing.
    let frames: Vec<(u64, Vec<u8>)> = [(0, 4), (33, 10), (66, 16), (500, 22), (533, 28)]
        .into_iter()
        .map(|(ms, x)| (ms, frame(w, h, x, 20)))
        .collect();
    let rvr = scratch("export.rvr");
    let mp4 = scratch("export.mp4");
    let _ = std::fs::remove_file(&mp4);
    write_recording(&rvr, w, h, &frames, 2000);

    let run = Command::new(env!("CARGO_BIN_EXE_raven-export"))
        .arg(&rvr)
        .arg("-o")
        .arg(&mp4)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let info = probe(&mp4);
    assert_eq!(field(&info, "codec_name"), "h264");
    assert_eq!(field(&info, "profile"), "Constrained Baseline");
    assert_eq!(field(&info, "width"), "96");
    assert_eq!(field(&info, "height"), "64");
    assert_eq!(field(&info, "nb_read_frames"), "5");
    assert_eq!(field(&info, "color_space"), "bt709");
    assert_eq!(field(&info, "color_range"), "tv");
    let duration: f64 = field(&info, "duration").parse().unwrap();
    assert!((duration - 2.0).abs() < 0.05, "duration {duration}");

    // Decoded back to RGB, every frame is close to what was recorded.
    let rgb = scratch("export.rgb");
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&mp4)
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
        ])
        .arg(&rgb)
        .output()
        .unwrap();
    assert!(
        decode.status.success(),
        "{}",
        String::from_utf8_lossy(&decode.stderr)
    );
    let decoded = std::fs::read(&rgb).unwrap();
    assert_eq!(decoded.len(), 5 * w * h * 3);
    for (n, ((_, rgba), got)) in frames.iter().zip(decoded.chunks(w * h * 3)).enumerate() {
        let error: u64 = rgba
            .chunks(4)
            .zip(got.chunks(3))
            .map(|(want, got)| {
                (0..3)
                    .map(|c| u64::from(want[c].abs_diff(got[c])))
                    .sum::<u64>()
            })
            .sum();
        let mean = error as f64 / (w * h * 3) as f64;
        assert!(mean < 4.0, "frame {n} is off by {mean:.2} on average");
    }
}
