//! Write a synthetic 1080p screen recording, for timing raven-export.
//!
//! ```sh
//! cargo run --release -p raven-export --example synthetic -- /tmp/synthetic.rvr
//! cargo run --release -p raven-export -- /tmp/synthetic.rvr
//! ```
//!
//! Five seconds at 30 Hz of the things a screen recording is made of: a
//! window of text scrolling, the window being dragged, a progress bar filling.

use std::fs::File;
use std::io::BufWriter;
use std::time::Duration;

const W: usize = 1920;
const H: usize = 1080;

fn hash(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

/// A pixel of a line of pseudo-text, in a cell grid of 10×20.
fn text(px: usize, py: usize) -> bool {
    let (line, col) = ((py / 20) as u64, (px / 10) as u64);
    let length = 20 + hash(line * 7919) % 90;
    if col >= length || hash(line * 31 + col).is_multiple_of(7) {
        return false; // past the end of the line, or a space
    }
    let (cx, cy) = (px % 10, py % 20);
    if cx >= 7 || !(4..16).contains(&cy) {
        return false;
    }
    hash(line * 1_000_003 + col * 1009 + (cx / 2) as u64 * 17 + ((cy - 4) / 3) as u64) & 1 == 1
}

fn draw(frame: &mut [u8], n: usize) {
    let scroll = (n.min(60) * 2) as isize;
    let window_x = 200 + n.clamp(60, 90).saturating_sub(60) * 8;
    let (window_y, window_w, window_h) = (120, 1200, 800);
    let progress = n.saturating_sub(90) * 12;
    for y in 0..H {
        for x in 0..W {
            let mut pixel = [38, 44, 58, 255]; // wallpaper
            if (0..32).contains(&y) {
                pixel = [22, 22, 31, 255]; // top bar
            }
            let inside = (window_x..window_x + window_w).contains(&x)
                && (window_y..window_y + window_h).contains(&y);
            if inside {
                let (wx, wy) = (x - window_x, y - window_y);
                pixel = if wy < 30 {
                    [52, 52, 66, 255] // title bar
                } else if wy > window_h - 40 {
                    if (20..1180).contains(&wx) && wy > window_h - 30 && wy < window_h - 14 {
                        if wx - 20 < progress {
                            [122, 162, 247, 255]
                        } else {
                            [60, 60, 74, 255]
                        }
                    } else {
                        [30, 30, 40, 255]
                    }
                } else if text(wx + 4, (wy as isize + scroll) as usize) {
                    [232, 232, 240, 255]
                } else {
                    [22, 22, 31, 255]
                };
            }
            frame[(y * W + x) * 4..(y * W + x) * 4 + 4].copy_from_slice(&pixel);
        }
    }
}

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: synthetic OUTPUT.rvr");
        std::process::exit(2);
    };
    let file = BufWriter::new(File::create(&path).expect("creating the recording"));
    let mut encoder = raven_rec::Encoder::new(file, W as u32, H as u32).expect("a 1080p header");
    let mut frame = vec![0u8; W * H * 4];
    let mut at = Duration::ZERO;
    for n in 0..150 {
        draw(&mut frame, n);
        encoder.push(at, &frame).expect("writing a frame");
        at += Duration::from_micros(33_333);
    }
    let file = encoder.finish(at).expect("finishing");
    drop(file);
    eprintln!(
        "wrote {path}: 150 frames of 1080p, {:.1} s",
        at.as_secs_f64()
    );
}
