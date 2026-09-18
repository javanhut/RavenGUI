//! Round trips through a decoder somebody else wrote: Symphonia's AAC-LC.
//!
//! The host has no ffmpeg, and a decoder that is not ours is the point: a
//! stream it decodes to the input, it decodes because the stream is right,
//! not because our mistakes cancel out. Each test encodes a signal, decodes
//! every frame, drops the priming and compares sample by sample.

use raven_aac::{Config, Encoder, PRIMING};
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::packet::Packet;
use symphonia::core::units::{Duration, Timestamp};

struct Encoded {
    config: Config,
    asc: Vec<u8>,
    frames: Vec<Vec<u8>>,
}

fn encode(config: Config, pcm: &[i16]) -> Encoded {
    let mut encoder = Encoder::new(config).unwrap();
    let asc = encoder.audio_specific_config();
    // Fed in uneven pieces, as a capture thread would.
    let mut frames = Vec::new();
    for piece in pcm.chunks(4801) {
        frames.extend(encoder.encode(piece));
    }
    frames.extend(encoder.finish());
    Encoded {
        config,
        asc,
        frames,
    }
}

/// Decode to interleaved samples at the input's 16-bit scale, priming gone.
fn decode(e: &Encoded) -> Vec<f32> {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_AAC)
        .with_extra_data(e.asc.clone().into_boxed_slice());
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .unwrap();
    let mut out: Vec<f32> = Vec::new();
    for (i, frame) in e.frames.iter().enumerate() {
        let packet = Packet::new(
            0,
            Timestamp::new(i as i64 * 1024),
            Duration::new(1024),
            frame.clone(),
        );
        let buf = decoder
            .decode(&packet)
            .unwrap_or_else(|err| panic!("frame {i} of {}: {err}", e.frames.len()));
        assert_eq!(buf.frames(), 1024);
        let mut samples: Vec<f32> = Vec::new();
        buf.copy_to_vec_interleaved(&mut samples);
        out.extend(samples);
    }
    let skip = PRIMING as usize * usize::from(e.config.channels);
    out.drain(..skip);
    out.iter_mut().for_each(|s| *s *= 32768.0);
    out
}

fn snr(input: &[i16], output: &[f32]) -> f64 {
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for (&x, &y) in input.iter().zip(output) {
        signal += f64::from(x) * f64::from(x);
        noise += (f64::from(x) - f64::from(y)).powi(2);
    }
    10.0 * (signal / noise.max(1e-9)).log10()
}

fn kbps(e: &Encoded, samples_per_channel: usize) -> f64 {
    let bytes: usize = e.frames.iter().map(Vec::len).sum();
    bytes as f64 * 8.0 / (samples_per_channel as f64 / f64::from(e.config.sample_rate)) / 1000.0
}

/// `window_sequence` of each frame, read straight from the bitstream.
fn sequences(e: &Encoded) -> Vec<u8> {
    e.frames
        .iter()
        .map(|f| {
            if e.config.channels == 2 {
                (f[1] >> 5) & 3 // id, tag, common_window, reserved bit
            } else {
                f[2] >> 6 // id, tag, global_gain, reserved bit
            }
        })
        .collect()
}

struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(self.0 >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0
    }
}

fn interleave(channels: &[Vec<f64>]) -> Vec<i16> {
    let n = channels[0].len();
    (0..n)
        .flat_map(|i| {
            channels
                .iter()
                .map(move |c| c[i].round().clamp(-32768.0, 32767.0) as i16)
        })
        .collect()
}

fn sine(rate: u32, seconds: f64, hz: f64, amplitude: f64) -> Vec<f64> {
    let n = (f64::from(rate) * seconds) as usize;
    (0..n)
        .map(|i| amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin())
        .collect()
}

/// A logarithmic sweep from `from` to `to` Hz.
fn chirp(rate: u32, seconds: f64, from: f64, to: f64, amplitude: f64) -> Vec<f64> {
    let n = (f64::from(rate) * seconds) as usize;
    let k = (to / from).ln() / seconds;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let phase = 2.0 * std::f64::consts::PI * from * ((k * t).exp() - 1.0) / k;
            amplitude * phase.sin()
        })
        .collect()
}

fn noise(rate: u32, seconds: f64, amplitude: f64, seed: u32) -> Vec<f64> {
    let mut lcg = Lcg(seed);
    let n = (f64::from(rate) * seconds) as usize;
    (0..n).map(|_| amplitude * lcg.next()).collect()
}

/// Sharp decaying clicks every 100 ms over near-silence.
fn clicks(rate: u32, seconds: f64) -> (Vec<f64>, Vec<usize>) {
    let n = (f64::from(rate) * seconds) as usize;
    let mut out = noise(rate, seconds, 30.0, 99);
    let period = rate as usize / 10;
    let mut at = Vec::new();
    let mut start = period / 2 + 137;
    while start + 2000 < n {
        at.push(start);
        for i in 0..2000 {
            let decay = (-(i as f64) / 150.0).exp();
            out[start + i] += 20_000.0 * decay * (i as f64 * 0.9).sin();
        }
        start += period;
    }
    (out, at)
}

struct Case {
    name: &'static str,
    rate: u32,
    channels: Vec<Vec<f64>>,
    /// Over the whole signal, abrupt start and end included…
    min_snr: f64,
    /// …and away from them, where the signal is steady.
    min_middle_snr: f64,
}

fn run(case: &Case) -> (Encoded, Vec<i16>, Vec<f32>) {
    let pcm = interleave(&case.channels);
    let config = Config::new(case.rate, case.channels.len() as u8);
    let e = encode(config, &pcm);
    let out = decode(&e);
    assert!(out.len() >= pcm.len(), "{}: decoded too little", case.name);
    let got = snr(&pcm, &out);
    let edge = 4096 * case.channels.len();
    let middle = snr(&pcm[edge..pcm.len() - edge], &out[edge..pcm.len() - edge]);
    let rate = kbps(&e, case.channels[0].len());
    eprintln!(
        "{:32} SNR {got:6.1} dB (middle {middle:6.1})  {rate:6.1} kb/s",
        case.name
    );
    assert!(
        got >= case.min_snr,
        "{}: SNR {got:.1} dB, wanted {}",
        case.name,
        case.min_snr
    );
    assert!(
        middle >= case.min_middle_snr,
        "{}: SNR {middle:.1} dB in the middle, wanted {}",
        case.name,
        case.min_middle_snr
    );
    (e, pcm, out)
}

#[test]
fn tones_and_sweeps_come_back_close() {
    for rate in [44_100, 48_000] {
        for case in [
            Case {
                name: "1 kHz sine, mono",
                rate,
                channels: vec![sine(rate, 2.0, 1000.0, 16_000.0)],
                min_snr: 30.0,
                min_middle_snr: 55.0,
            },
            Case {
                name: "440 + 3 kHz sines, stereo",
                rate,
                channels: vec![
                    sine(rate, 2.0, 440.0, 12_000.0),
                    sine(rate, 2.0, 3000.0, 8_000.0),
                ],
                min_snr: 30.0,
                min_middle_snr: 50.0,
            },
            Case {
                name: "chirp 50 Hz-15 kHz, mono",
                rate,
                channels: vec![chirp(rate, 3.0, 50.0, 15_000.0, 12_000.0)],
                min_snr: 35.0,
                min_middle_snr: 50.0,
            },
            Case {
                name: "chirp, stereo, one side quieter",
                rate,
                channels: vec![
                    chirp(rate, 3.0, 50.0, 15_000.0, 12_000.0),
                    chirp(rate, 3.0, 50.0, 15_000.0, 6_000.0),
                ],
                min_snr: 35.0,
                min_middle_snr: 50.0,
            },
        ] {
            run(&case);
        }
    }
}

#[test]
fn noise_keeps_its_level_and_the_bitrate_its_target() {
    for rate in [44_100, 48_000] {
        let case = Case {
            name: "white noise, stereo",
            rate,
            channels: vec![noise(rate, 4.0, 8000.0, 1), noise(rate, 4.0, 8000.0, 2)],
            min_snr: 3.0,
            min_middle_snr: 3.0,
        };
        let (e, pcm, out) = run(&case);
        let rate_kbps = kbps(&e, case.channels[0].len());
        assert!((115.0..=135.0).contains(&rate_kbps), "{rate_kbps} kb/s");
        // Noise is coded as noise, not waveform: its level must survive.
        let energy = |s: &mut dyn Iterator<Item = f64>| s.map(|v| v * v).sum::<f64>();
        let a = energy(&mut pcm.iter().map(|&v| f64::from(v)));
        let b = energy(&mut out[..pcm.len()].iter().map(|&v| f64::from(v)));
        // Less what lies above the cut-off (15.8 kHz at 64 kb/s a channel):
        // 1 dB of white noise at 48 kHz, 1.5 dB at 44.1.
        let db = 10.0 * (b / a).log10();
        assert!((-2.5..0.5).contains(&db), "level changed by {db:.2} dB");

        let mono = Case {
            name: "white noise, mono",
            rate,
            channels: vec![noise(rate, 4.0, 8000.0, 3)],
            min_snr: 3.0,
            min_middle_snr: 3.0,
        };
        let (e, _, _) = run(&mono);
        let rate_kbps = kbps(&e, mono.channels[0].len());
        assert!((57.0..=68.0).contains(&rate_kbps), "{rate_kbps} kb/s");
    }
}

#[test]
fn music_like_material_lands_on_the_target_bitrate() {
    // A chord of harmonics with a little noise: denser than a sine, the
    // kind of frame the rate loop spends its bits on.
    let rate = 48_000;
    let mut left = vec![0.0; rate as usize * 4];
    let mut right = left.clone();
    for (k, hz) in [220.0, 277.2, 329.6, 440.0].iter().enumerate() {
        for h in 1..12 {
            let a = 2500.0 / f64::from(h);
            let s = sine(rate, 4.0, hz * f64::from(h), a);
            for (i, v) in s.iter().enumerate() {
                left[i] += v * if k % 2 == 0 { 1.0 } else { 0.6 };
                right[i] += v * if k % 2 == 0 { 0.6 } else { 1.0 };
            }
        }
    }
    let hiss = noise(rate, 4.0, 300.0, 5);
    for (i, v) in hiss.iter().enumerate() {
        left[i] += v;
        right[i] -= v;
    }
    let case = Case {
        name: "chord with hiss, stereo",
        rate,
        channels: vec![left, right],
        min_snr: 13.0,
        min_middle_snr: 13.0,
    };
    let (e, _, _) = run(&case);
    let rate_kbps = kbps(&e, case.channels[0].len());
    assert!((118.0..=134.0).contains(&rate_kbps), "{rate_kbps} kb/s");
}

#[test]
fn silence_is_silent_and_nearly_free() {
    for channels in [1usize, 2] {
        let case = Case {
            name: "silence",
            rate: 48_000,
            channels: vec![vec![0.0; 48_000]; channels],
            min_snr: f64::NEG_INFINITY,
            min_middle_snr: f64::NEG_INFINITY,
        };
        let (e, _, out) = run(&case);
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(kbps(&e, 48_000) < 4.0);
    }
}

#[test]
fn clicks_switch_to_short_blocks_and_do_not_pre_echo() {
    // Mono at 64 kb/s spends its bits on the clicks' onsets; stereo at 128
    // kb/s codes identical channels as mid alone, with bits to spare.
    for (rate, channels, min_snr) in [(48_000u32, 1usize, 9.0), (44_100, 2, 25.0)] {
        let (signal, at) = clicks(rate, 2.0);
        let case = Case {
            name: "click train",
            rate,
            channels: vec![signal; channels],
            min_snr,
            min_middle_snr: min_snr,
        };
        let (e, pcm, out) = run(&case);
        let seq = sequences(&e);
        let short = seq.iter().filter(|&&s| s == 2).count();
        assert!(
            short >= at.len(),
            "only {short} short frames for {} clicks",
            at.len()
        );
        // Before each click, the decoded signal stays near the quiet floor:
        // the noise has not been smeared ahead of the attack. The last 6 ms
        // are left out: that far, one short window's noise does reach back,
        // and it is premasked (the ear does not hear it before the click).
        let n = channels;
        for &click in &at {
            let ms = rate as usize / 1000;
            let before = click - 25 * ms..click - 6 * ms;
            let err: f64 = before
                .clone()
                .map(|i| (f64::from(out[i * n]) - f64::from(pcm[i * n])).powi(2))
                .sum::<f64>()
                / before.len() as f64;
            let rms = err.sqrt();
            assert!(
                rms < 60.0,
                "pre-echo of {rms:.0} before the click at {click}"
            );
            // And the click itself is where it was: the decoded burst lines
            // up with the input best at no shift at all.
            let corr = |lag: isize| {
                (click..click + 400)
                    .map(|i| {
                        f64::from(pcm[i * n]) * f64::from(out[(i as isize + lag) as usize * n])
                    })
                    .sum::<f64>()
            };
            let best = (-8..=8)
                .max_by(|&a, &b| corr(a).total_cmp(&corr(b)))
                .unwrap();
            assert_eq!(best, 0, "the click at {click} moved by {best}");
        }
    }
}

#[test]
fn a_steady_tone_stays_in_long_blocks() {
    let pcm = interleave(&[sine(48_000, 2.0, 1000.0, 16_000.0)]);
    let e = encode(Config::new(48_000, 1), &pcm);
    let seq = sequences(&e);
    // The tone's own onset may switch once; after that, long blocks only.
    assert!(seq[3..].iter().all(|&s| s == 0), "{seq:?}");
}

#[test]
fn other_rates_decode() {
    for rate in [8_000, 16_000, 22_050, 32_000, 96_000] {
        let mut config = Config::new(rate, 2);
        config.bitrate = config.bitrate.min(rate * 3);
        let pcm = interleave(&[
            sine(rate, 1.0, 300.0, 10_000.0),
            noise(rate, 1.0, 3000.0, 7),
        ]);
        let e = encode(config, &pcm);
        let out = decode(&e);
        let left_in: Vec<i16> = pcm.iter().step_by(2).copied().collect();
        let left_out: Vec<f32> = out.iter().step_by(2).copied().collect();
        let got = snr(&left_in, &left_out);
        eprintln!("{rate} Hz: left SNR {got:.1} dB");
        assert!(got > 15.0, "{rate} Hz: {got:.1} dB");
    }
}
