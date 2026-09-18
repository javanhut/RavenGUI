//! Files written, then read back: by a box walker here, which checks the
//! index points at exactly the bytes pushed, and by Symphonia, a demuxer and
//! AAC decoder somebody else wrote.

use std::ops::Range;
use std::path::PathBuf;

use raven_mp4::{Audio, Mp4, TIMESCALE, Video};

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("raven-mp4-{}-{name}", std::process::id()))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The boxes directly inside `range`, as (kind, body).
fn children(b: &[u8], range: Range<usize>) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let mut at = range.start;
    while at + 8 <= range.end {
        let mut size = u64::from(u32_at(b, at));
        let kind = String::from_utf8_lossy(&b[at + 4..at + 8]).into_owned();
        let mut header = 8;
        if size == 1 {
            size = u64_at(b, at + 8);
            header = 16;
        }
        let end = at + size as usize;
        assert!(end <= range.end, "{kind} overruns its parent");
        out.push((kind, at + header..end));
        at = end;
    }
    assert_eq!(at, range.end, "boxes do not add up to their parent");
    out
}

fn child(b: &[u8], range: Range<usize>, kind: &str) -> Option<Range<usize>> {
    children(b, range)
        .into_iter()
        .find(|(k, _)| k == kind)
        .map(|(_, r)| r)
}

fn path(b: &[u8], range: Range<usize>, kinds: &[&str]) -> Range<usize> {
    kinds.iter().fold(range, |r, k| {
        child(b, r, k).unwrap_or_else(|| panic!("no {k}"))
    })
}

#[derive(Debug)]
struct Track {
    handler: String,
    timescale: u32,
    media_duration: u64,
    /// Samples: file offset and size.
    samples: Vec<(u64, u32)>,
    /// Chunk offsets.
    chunks: Vec<u64>,
    stts_total: u64,
    /// segment_duration (movie ticks), media_time.
    edit: Option<(u32, i32)>,
    entry: String,
}

struct File {
    bytes: Vec<u8>,
    brand: String,
    movie_duration: u32,
    tracks: Vec<Track>,
    mdat: Range<usize>,
}

fn read(p: &PathBuf) -> File {
    let bytes = std::fs::read(p).unwrap();
    let top = children(&bytes, 0..bytes.len());
    let kinds: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, ["ftyp", "mdat", "moov"]);
    let brand = String::from_utf8_lossy(&bytes[top[0].1.start..top[0].1.start + 4]).into_owned();
    let mdat = top[1].1.clone();
    let moov = top[2].1.clone();
    let mvhd = child(&bytes, moov.clone(), "mvhd").unwrap();
    let movie_duration = u32_at(&bytes, mvhd.start + 16);
    let mut tracks = Vec::new();
    for (kind, trak) in children(&bytes, moov) {
        if kind != "trak" {
            continue;
        }
        let mdia = path(&bytes, trak.clone(), &["mdia"]);
        let hdlr = child(&bytes, mdia.clone(), "hdlr").unwrap();
        let handler = String::from_utf8_lossy(&bytes[hdlr.start + 8..hdlr.start + 12]).into_owned();
        let mdhd = child(&bytes, mdia.clone(), "mdhd").unwrap();
        assert_eq!(bytes[mdhd.start], 1, "mdhd version 1");
        let timescale = u32_at(&bytes, mdhd.start + 20);
        let media_duration = u64_at(&bytes, mdhd.start + 24);
        let stbl = path(&bytes, mdia, &["minf", "stbl"]);
        let stsd = child(&bytes, stbl.clone(), "stsd").unwrap();
        let entry = String::from_utf8_lossy(&bytes[stsd.start + 12..stsd.start + 16]).into_owned();
        let stts = child(&bytes, stbl.clone(), "stts").unwrap();
        let mut stts_total = 0u64;
        for i in 0..u32_at(&bytes, stts.start + 4) as usize {
            let at = stts.start + 8 + 8 * i;
            stts_total += u64::from(u32_at(&bytes, at)) * u64::from(u32_at(&bytes, at + 4));
        }
        let stsz = child(&bytes, stbl.clone(), "stsz").unwrap();
        let count = u32_at(&bytes, stsz.start + 8) as usize;
        let sizes: Vec<u32> = (0..count)
            .map(|i| u32_at(&bytes, stsz.start + 12 + 4 * i))
            .collect();
        let chunks: Vec<u64> = if let Some(stco) = child(&bytes, stbl.clone(), "stco") {
            (0..u32_at(&bytes, stco.start + 4) as usize)
                .map(|i| u64::from(u32_at(&bytes, stco.start + 8 + 4 * i)))
                .collect()
        } else {
            let co64 = child(&bytes, stbl.clone(), "co64").unwrap();
            (0..u32_at(&bytes, co64.start + 4) as usize)
                .map(|i| u64_at(&bytes, co64.start + 8 + 8 * i))
                .collect()
        };
        // stsc: runs of (first chunk, samples per chunk), expanded.
        let stsc = child(&bytes, stbl, "stsc").unwrap();
        let runs: Vec<(usize, usize)> = (0..u32_at(&bytes, stsc.start + 4) as usize)
            .map(|i| {
                let at = stsc.start + 8 + 12 * i;
                (u32_at(&bytes, at) as usize, u32_at(&bytes, at + 4) as usize)
            })
            .collect();
        let mut samples = Vec::new();
        let mut next = sizes.iter();
        for (c, &offset) in chunks.iter().enumerate() {
            let per = runs
                .iter()
                .rev()
                .find(|(first, _)| *first <= c + 1)
                .unwrap()
                .1;
            let mut at = offset;
            for _ in 0..per {
                let &size = next.next().expect("stsc describes more samples than stsz");
                samples.push((at, size));
                at += u64::from(size);
            }
        }
        assert!(
            next.next().is_none(),
            "stsc describes fewer samples than stsz"
        );
        let edit = child(&bytes, trak, "edts").map(|edts| {
            let elst = child(&bytes, edts, "elst").unwrap();
            assert_eq!(u32_at(&bytes, elst.start + 4), 1);
            (
                u32_at(&bytes, elst.start + 8),
                u32_at(&bytes, elst.start + 12) as i32,
            )
        });
        tracks.push(Track {
            handler,
            timescale,
            media_duration,
            samples,
            chunks,
            stts_total,
            edit,
            entry,
        });
    }
    File {
        bytes,
        brand,
        movie_duration,
        tracks,
        mdat,
    }
}

/// Every sample lies in `mdat`, none overlap, and each holds what was pushed.
fn check_samples(file: &File, track: usize, pushed: &[Vec<u8>]) {
    let t = &file.tracks[track];
    assert_eq!(t.samples.len(), pushed.len());
    for (&(offset, size), want) in t.samples.iter().zip(pushed) {
        let range = offset as usize..offset as usize + size as usize;
        assert!(range.start >= file.mdat.start && range.end <= file.mdat.end);
        assert_eq!(&file.bytes[range], want.as_slice());
    }
}

const SPS: [u8; 5] = [0x67, 66, 0xC0, 41, 0xAA];

fn video() -> Video {
    Video {
        width: 64,
        height: 48,
        sps: SPS.to_vec(),
        pps: vec![0x68, 0xCE],
    }
}

/// Video frames as they are stored: 4-byte length, then the NAL unit.
fn stored(nal: &[u8]) -> Vec<u8> {
    [&(nal.len() as u32).to_be_bytes()[..], nal].concat()
}

fn sine(rate: u32, seconds: f64, channels: usize) -> Vec<i16> {
    let n = (f64::from(rate) * seconds) as usize;
    (0..n)
        .flat_map(|i| {
            let t = i as f64 / f64::from(rate);
            let s = (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16;
            std::iter::repeat_n(s, channels)
        })
        .collect()
}

/// AAC frames of `pcm`, by Raven's own encoder.
fn aac(rate: u32, channels: u8, pcm: &[i16]) -> (Audio, Vec<Vec<u8>>) {
    let mut encoder = raven_aac::Encoder::new(raven_aac::Config::new(rate, channels)).unwrap();
    let audio = Audio {
        sample_rate: rate,
        channels,
        config: encoder.audio_specific_config(),
        priming: raven_aac::PRIMING,
    };
    let mut frames = encoder.encode(pcm);
    frames.extend(encoder.finish());
    (audio, frames)
}

#[test]
fn video_alone_is_indexed_exactly() {
    let p = scratch("video.mp4");
    let mut mp4 = Mp4::create(&p, Some(video()), None).unwrap();
    let nals = [vec![0x65, 1, 2, 3], vec![0x41, 9], vec![0x41, 8]];
    mp4.push_video(0, &[nals[0].clone()], true).unwrap();
    mp4.push_video(3000, &[nals[1].clone()], false).unwrap();
    mp4.push_video(3000, &[nals[2].clone()], false).unwrap();
    let size = mp4.finish(90_000).unwrap();
    let file = read(&p);
    let _ = std::fs::remove_file(&p);

    assert_eq!(file.bytes.len() as u64, size);
    assert_eq!(file.brand, "isom");
    // Three samples of 8, 6 and 6 bytes in the mdat's body.
    assert_eq!(file.mdat.len(), 8 + 6 + 6);
    assert_eq!(file.tracks.len(), 1);
    let t = &file.tracks[0];
    assert_eq!((t.handler.as_str(), t.entry.as_str()), ("vide", "avc1"));
    assert_eq!(t.timescale, TIMESCALE);
    // The second frame at 3000 moves to 3001, one tick after its twin.
    assert_eq!(t.media_duration, 90_000);
    assert_eq!(t.stts_total, 90_000);
    assert_eq!(file.movie_duration, 1000);
    assert!(t.edit.is_none());
    let pushed: Vec<Vec<u8>> = nals.iter().map(|n| stored(n)).collect();
    check_samples(&file, 0, &pushed);
}

#[test]
fn video_and_audio_interleave_and_skip_the_priming() {
    let rate = 48_000;
    let seconds = 3.0;
    let (audio, frames) = aac(rate, 2, &sine(rate, seconds, 2));
    let p = scratch("both.mp4");
    let mut mp4 = Mp4::create(&p, Some(video()), Some(audio)).unwrap();
    // 30 frames a second of video, pushed in time order with the audio.
    let mut video_frames = Vec::new();
    let mut a = frames.iter();
    let mut audio_time = -1024.0 / f64::from(rate);
    for n in 0..(seconds * 30.0) as u64 {
        let t = n as f64 / 30.0;
        while audio_time <= t {
            let Some(f) = a.next() else { break };
            mp4.push_audio(f).unwrap();
            audio_time += 1024.0 / f64::from(rate);
        }
        let nal = vec![if n % 30 == 0 { 0x65 } else { 0x41 }, n as u8, 7, 7, 7];
        mp4.push_video(n * 3000, std::slice::from_ref(&nal), n % 30 == 0)
            .unwrap();
        video_frames.push(stored(&nal));
    }
    for f in a {
        mp4.push_audio(f).unwrap();
    }
    let size = mp4.finish((seconds * 90_000.0) as u64).unwrap();
    let file = read(&p);
    assert_eq!(file.bytes.len() as u64, size);
    assert_eq!(file.brand, "isom");
    assert_eq!(file.tracks.len(), 2);
    let (v, s) = (&file.tracks[0], &file.tracks[1]);
    assert_eq!((v.handler.as_str(), s.handler.as_str()), ("vide", "soun"));
    assert_eq!(s.entry, "mp4a");
    assert_eq!(s.timescale, rate);
    assert_eq!(v.stts_total, 270_000);
    assert_eq!(s.media_duration, frames.len() as u64 * 1024);
    assert_eq!(s.stts_total, s.media_duration);
    // The edit list starts past the priming and runs for the real sound.
    assert_eq!(s.edit, Some((3000, 1024)));
    assert_eq!(file.movie_duration, 3000);
    check_samples(&file, 0, &video_frames);
    check_samples(&file, 1, &frames);

    // Interleaved: chunks of either track alternate through the file, a
    // few per second, rather than one track after the other.
    let mut all: Vec<(u64, usize)> = v.chunks.iter().map(|&o| (o, 0)).collect();
    all.extend(s.chunks.iter().map(|&o| (o, 1)));
    all.sort();
    let switches = all.windows(2).filter(|w| w[0].1 != w[1].1).count();
    assert!(switches >= 8, "only {switches} switches between tracks");
    assert!(
        s.chunks.len() >= 5 && s.chunks.len() < frames.len() / 4,
        "{} audio chunks",
        s.chunks.len()
    );

    // Symphonia finds both tracks and decodes the sound.
    let decoded = decode_audio(&p);
    let _ = std::fs::remove_file(&p);
    check_sound(&decoded, &sine(rate, seconds, 2), 2);
}

#[test]
fn audio_alone_is_an_m4a() {
    for (rate, channels) in [(44_100, 1u8), (48_000, 2)] {
        let seconds = 2.0;
        let pcm = sine(rate, seconds, usize::from(channels));
        let (audio, frames) = aac(rate, channels, &pcm);
        let p = scratch(&format!("alone-{rate}.m4a"));
        let mut mp4 = Mp4::create(&p, None, Some(audio)).unwrap();
        for f in &frames {
            mp4.push_audio(f).unwrap();
        }
        let samples = pcm.len() as u64 / u64::from(channels);
        let size = mp4
            .finish(samples * u64::from(TIMESCALE) / u64::from(rate))
            .unwrap();
        let file = read(&p);
        assert_eq!(file.bytes.len() as u64, size);
        assert_eq!(file.brand, "M4A ");
        assert_eq!(file.tracks.len(), 1);
        let s = &file.tracks[0];
        assert_eq!(s.handler, "soun");
        assert_eq!(s.edit, Some((2000, 1024)));
        assert_eq!(file.movie_duration, 2000);
        check_samples(&file, 0, &frames);
        let decoded = decode_audio(&p);
        let _ = std::fs::remove_file(&p);
        check_sound(&decoded, &pcm, usize::from(channels));
    }
}

/// Demux and decode the first audio track with Symphonia: interleaved
/// samples at 16-bit scale. Symphonia reads the edit list but does not act
/// on it, so the priming is still there.
fn decode_audio(p: &PathBuf) -> Vec<f32> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let source = MediaSourceStream::new(
        Box::new(std::fs::File::open(p).unwrap()),
        Default::default(),
    );
    let mut format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let track = format.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .unwrap()
        .audio()
        .unwrap()
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .unwrap();
    let mut out = Vec::new();
    while let Some(packet) = format.next_packet().unwrap() {
        if packet.track_id != id {
            continue;
        }
        let buf = decoder.decode(&packet).unwrap();
        let mut samples: Vec<f32> = Vec::new();
        buf.copy_to_vec_interleaved(&mut samples);
        out.extend(samples.iter().map(|s| s * 32768.0));
    }
    out
}

fn check_sound(decoded: &[f32], pcm: &[i16], channels: usize) {
    let skip = raven_aac::PRIMING as usize * channels;
    let decoded = &decoded[skip..];
    assert!(decoded.len() >= pcm.len());
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for (&x, &y) in pcm.iter().zip(decoded) {
        signal += f64::from(x).powi(2);
        noise += (f64::from(x) - f64::from(y)).powi(2);
    }
    let snr = 10.0 * (signal / noise).log10();
    assert!(snr > 30.0, "SNR {snr:.1} dB");
}
