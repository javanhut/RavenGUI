//! Time the encoder on a minute of synthetic 48 kHz stereo.
//!
//! ```sh
//! cargo run --release -p raven-aac --example speed
//! ```
//!
//! The material is the hard kind: a chord, noise, and a click every 250 ms,
//! so the rate loop works and short blocks switch in and out.

use std::time::Instant;

fn main() {
    let rate = 48_000u32;
    let seconds = 60;
    let mut s = 1u32;
    let mut pcm = Vec::with_capacity((rate * seconds * 2) as usize);
    for i in 0..(rate * seconds) as usize {
        let t = i as f64 / f64::from(rate);
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let hiss = f64::from((s >> 16) as i16) / 32768.0 * 600.0;
        let chord: f64 = [220.0, 277.2, 329.6, 440.0, 1320.0]
            .iter()
            .map(|hz| 3000.0 * (2.0 * std::f64::consts::PI * hz * t).sin())
            .sum();
        let since = i % (rate as usize / 4);
        let click = 12_000.0 * (-(since as f64) / 200.0).exp() * (since as f64 * 0.7).sin();
        pcm.push((chord + hiss + click) as i16);
        pcm.push((chord * 0.7 - hiss + click) as i16);
    }
    let mut encoder = raven_aac::Encoder::new(raven_aac::Config::new(rate, 2)).unwrap();
    let started = Instant::now();
    let mut bytes = 0usize;
    for piece in pcm.chunks(4800) {
        bytes += encoder.encode(piece).iter().map(Vec::len).sum::<usize>();
    }
    bytes += encoder.finish().iter().map(Vec::len).sum::<usize>();
    let took = started.elapsed().as_secs_f64();
    println!(
        "{seconds} s of 48 kHz stereo in {took:.2} s: {:.0}x real time, {:.1} kb/s",
        f64::from(seconds) / took,
        bytes as f64 * 8.0 / f64::from(seconds) / 1000.0
    );
}
