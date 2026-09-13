//! Conformance: ffmpeg's decoder, fed our stream, must reproduce the encoder's
//! own reconstruction bit for bit.
//!
//! ffmpeg is a test oracle only; nothing in the encoder uses it. Where it is
//! not installed these tests say so and pass, since a missing oracle is not a
//! broken encoder.

use std::path::PathBuf;
use std::process::Command;

use raven_h264::{Config, Encoder, Picture, annex_b};

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Decode an Annex B stream to raw I420 with ffmpeg.
fn decode(stream: &[u8], name: &str) -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let input = dir.join(format!("{name}.264"));
    let output = dir.join(format!("{name}.yuv"));
    std::fs::write(&input, stream).unwrap();
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "h264", "-i"])
        .arg(&input)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg refused the stream: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::read(&output).unwrap()
}

/// A picture with something in it: gradients, a hard edge, and noise.
fn picture(width: usize, height: usize, seed: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut state = seed.wrapping_mul(2_654_435_761) | 1;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state % 24) as u8
    };
    let mut y = Vec::with_capacity(width * height);
    for row in 0..height {
        for col in 0..width {
            let base = if col > width / 2 {
                200
            } else {
                30 + (row * 3) as u8 % 100
            };
            y.push(base.saturating_add(noise()).clamp(16, 235));
        }
    }
    let (cw, ch) = (width / 2, height / 2);
    let cb = (0..cw * ch).map(|i| 90 + (i % cw) as u8 % 60).collect();
    let cr = (0..cw * ch).map(|i| 160 - (i / cw) as u8 % 60).collect();
    (y, cb, cr)
}

/// Encode `frames` pictures (key frames where `keys` says) and check ffmpeg
/// decodes every one to what the encoder reconstructed.
fn check(name: &str, config: Config, frames: &[(Vec<u8>, Vec<u8>, Vec<u8>)], keys: &[bool]) {
    if !have_ffmpeg() {
        eprintln!("ffmpeg is not installed; skipping {name}");
        return;
    }
    let mut encoder = Encoder::new(config).unwrap();
    let mut nals = vec![encoder.sps().to_vec(), encoder.pps().to_vec()];
    let mut expected = Vec::new();
    for ((y, cb, cr), &key) in frames.iter().zip(keys) {
        let encoded = encoder.encode(&Picture { y, cb, cr }, key).unwrap();
        nals.extend(encoded.nals);
        expected.extend(encoder.reconstruction_i420());
    }
    let decoded = decode(&annex_b(&nals), name);
    assert_eq!(
        decoded.len(),
        expected.len(),
        "{name}: frame count or size differs"
    );
    let frame = (config.width * config.height * 3 / 2) as usize;
    for (n, (got, want)) in decoded
        .chunks(frame)
        .zip(expected.chunks(frame))
        .enumerate()
    {
        if got != want {
            let at = got.iter().zip(want).position(|(a, b)| a != b).unwrap();
            panic!("{name}: frame {n} differs first at byte {at}");
        }
    }
}

type Frame = (Vec<u8>, Vec<u8>, Vec<u8>);

/// `frame` moved by (`dx`, `dy`) luma samples, edges repeated in.
fn shifted(frame: &Frame, width: usize, height: usize, dx: isize, dy: isize) -> Frame {
    let shift = |plane: &[u8], w: usize, h: usize, dx: isize, dy: isize| {
        (0..w * h)
            .map(|i| {
                let x = (i % w) as isize - dx;
                let y = (i / w) as isize - dy;
                let (x, y) = (x.clamp(0, w as isize - 1), y.clamp(0, h as isize - 1));
                plane[y as usize * w + x as usize]
            })
            .collect::<Vec<u8>>()
    };
    (
        shift(&frame.0, width, height, dx, dy),
        shift(&frame.1, width / 2, height / 2, dx / 2, dy / 2),
        shift(&frame.2, width / 2, height / 2, dx / 2, dy / 2),
    )
}

/// A desktop's worth of events: a window moves, nothing happens, something
/// repaints, the page scrolls, the chroma moves an odd distance.
fn desktop(width: usize, height: usize) -> Vec<Frame> {
    let first = picture(width, height, 7);
    let moved = shifted(&first, width, height, 6, 4);
    let mut repainted = moved.clone();
    for row in 8..24 {
        for col in 10..40 {
            repainted.0[row * width + col] = 180;
        }
    }
    let scrolled = shifted(&repainted, width, height, 0, -12);
    let odd = shifted(&scrolled, width, height, 3, 1);
    vec![first, moved.clone(), moved, repainted, scrolled, odd]
}

#[test]
fn p_frames_decode_to_the_reconstruction() {
    for (w, h) in [(128, 96), (100, 60)] {
        let frames = desktop(w, h);
        for qp in [0, 20, 32, 48] {
            let config = Config {
                qp,
                ..Config::new(w as u32, h as u32)
            };
            check(
                &format!("p{w}x{h}q{qp}"),
                config,
                &frames,
                &[true, false, false, false, false, false],
            );
        }
    }
}

#[test]
fn an_unchanged_frame_is_nearly_free() {
    // The first P frame after a lossy key frame spends bits sharpening what
    // the key frame blurred; once that has converged, a frame with nothing
    // new in it is a slice header and one skip run.
    let (w, h) = (320, 240);
    let (y, cb, cr) = picture(w, h, 3);
    let mut encoder = Encoder::new(Config::new(w as u32, h as u32)).unwrap();
    let picture = Picture {
        y: &y,
        cb: &cb,
        cr: &cr,
    };
    let sizes: Vec<usize> = (0..6)
        .map(|_| encoder.encode(&picture, false).unwrap().nals[0].len())
        .collect();
    let last = *sizes.last().unwrap();
    assert!(last < 16, "unchanged frames took {sizes:?} bytes");
}

#[test]
fn every_quantiser_decodes_to_the_reconstruction() {
    let (w, h) = (64, 48);
    let frames: Vec<_> = (1..=2).map(|seed| picture(w, h, seed)).collect();
    for qp in [0, 1, 10, 17, 24, 30, 36, 42, 51] {
        let config = Config {
            qp,
            ..Config::new(w as u32, h as u32)
        };
        check(&format!("qp{qp}"), config, &frames, &[true, false]);
    }
}

#[test]
fn cropped_frames_decode_to_the_reconstruction() {
    let (w, h) = (100, 60);
    let frames: Vec<_> = (1..=3).map(|seed| picture(w, h, seed)).collect();
    check(
        "cropped",
        Config::new(w as u32, h as u32),
        &frames,
        &[true, false, true],
    );
}
