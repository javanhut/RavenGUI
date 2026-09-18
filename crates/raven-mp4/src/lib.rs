//! Raven's own MP4 writer: an H.264 video track, an AAC audio track, or both.
//!
//! ISO/IEC 14496-12 (the container), -14 (MP4 and its `esds`) and -15 (AVC
//! in it), which are fixed documents, so this does not rot. It started life
//! inside raven-export and moved out when Raven Camera needed the same file
//! with sound in it.
//!
//! Samples are written into `mdat` as they arrive and the index — `moov` —
//! goes at the end, when every sample's size and time is known. `mdat`
//! carries a 64-bit size so a long recording cannot outgrow it.
//!
//! Video timing is whatever the caller's was: a screen that sat still for
//! ten seconds is one frame lasting ten seconds, and `stts` says so. Audio is
//! AAC, 1024 samples a frame, and the encoder's priming samples — the
//! silence every AAC decoder puts out before the first real sample — are
//! skipped by an edit list, so sound and picture start together.
//!
//! With both tracks, samples are not written the moment they arrive: each
//! track's samples wait in memory until they make a chunk of about half a
//! second, and chunks go to the file in time order. A player reading the
//! file front to back then finds the sound for a stretch of picture next to
//! it, instead of seeking between two halves of the file. Push samples in
//! roughly time order and what is held stays small; a track that runs far
//! ahead is written anyway once a few tens of megabytes wait.
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! use raven_mp4::{Audio, Mp4, TIMESCALE};
//! let audio = Audio { sample_rate: 48_000, channels: 2, config: vec![0x11, 0x90], priming: 1024 };
//! let mut mp4 = Mp4::create("talk.m4a".as_ref(), None, Some(audio))?;
//! # let frames: Vec<Vec<u8>> = Vec::new();
//! for frame in &frames {
//!     mp4.push_audio(frame)?;
//! }
//! mp4.finish(10 * u64::from(TIMESCALE))?; // ten seconds of sound
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Ticks per second of video time: the usual one for video. Video sample
/// times and the end time given to [`Mp4::finish`] are in these.
pub const TIMESCALE: u32 = 90_000;

/// Samples per channel in an AAC-LC frame.
pub const AAC_FRAME_LEN: u32 = 1024;

/// Ticks per second in the movie header, where only the total is kept.
const MOVIE_TIMESCALE: u32 = 1_000;

/// Where `mdat`'s 64-bit size lives: after `ftyp` (32 bytes: its header, the
/// major brand and version, four compatible brands) and the eight bytes of
/// `mdat`'s own compact header.
const MDAT_SIZE_AT: u64 = 32 + 8;

/// Seconds of one track a chunk holds before the other track gets a turn.
const CHUNK_SECONDS: f64 = 0.5;

/// Bytes held back for interleaving before they are written regardless.
const MOST_PENDING: usize = 32 << 20;

/// An H.264 track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Video {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The sequence parameter set, as a NAL unit without start code.
    pub sps: Vec<u8>,
    /// The picture parameter set, likewise.
    pub pps: Vec<u8>,
}

/// An AAC track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    /// Samples a second.
    pub sample_rate: u32,
    /// Channels, 1 or 2.
    pub channels: u8,
    /// The `AudioSpecificConfig`, as `raven_aac::Encoder::audio_specific_config`
    /// gives it.
    pub config: Vec<u8>,
    /// Samples the decoder produces before the first real one, skipped by
    /// the edit list: `raven_aac::PRIMING`.
    pub priming: u32,
}

/// One sample, written.
#[derive(Debug, Clone, Copy)]
struct Sample {
    size: u32,
    /// Start, in the track's timescale.
    start: u64,
    sync: bool,
}

/// One sample, waiting for its chunk.
#[derive(Debug)]
struct Waiting {
    data: Vec<u8>,
    /// Presentation time in seconds, for choosing what to write next.
    time: f64,
}

#[derive(Debug)]
enum Kind {
    Video(Video),
    Audio(Audio),
}

#[derive(Debug)]
struct Track {
    kind: Kind,
    samples: Vec<Sample>,
    /// File offset and sample count of each chunk.
    chunks: Vec<(u64, u32)>,
    waiting: VecDeque<Waiting>,
    /// Samples pushed, written or waiting.
    pushed: u64,
    /// The last pushed sample's start, for keeping video time moving.
    last_start: Option<u64>,
}

impl Track {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            samples: Vec::new(),
            chunks: Vec::new(),
            waiting: VecDeque::new(),
            pushed: 0,
            last_start: None,
        }
    }

    fn timescale(&self) -> u32 {
        match &self.kind {
            Kind::Video(_) => TIMESCALE,
            Kind::Audio(a) => a.sample_rate,
        }
    }
}

/// An MP4 being written. Every track given to [`Mp4::create`] should get at
/// least one sample.
#[derive(Debug)]
pub struct Mp4 {
    out: BufWriter<File>,
    /// Video first, when there is video.
    tracks: Vec<Track>,
    video: Option<usize>,
    audio: Option<usize>,
    position: u64,
    waiting_bytes: usize,
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(body.len() + 4);
    inner.extend_from_slice(&((u32::from(version) << 24) | flags).to_be_bytes());
    inner.extend_from_slice(body);
    boxed(kind, &inner)
}

/// An MPEG-4 descriptor (ISO/IEC 14496-1, 8.3.3): tag, size, body.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    // The size in 7-bit groups, high first, continuation bit on all but last.
    let len = body.len() as u32;
    let mut groups = vec![(len & 0x7F) as u8];
    let mut rest = len >> 7;
    while rest > 0 {
        groups.push((rest & 0x7F) as u8 | 0x80);
        rest >>= 7;
    }
    out.extend(groups.iter().rev());
    out.extend_from_slice(body);
    out
}

/// The unity transform every header carries.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn put(body: &mut Vec<u8>, bytes: &[u8]) {
    body.extend_from_slice(bytes);
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_owned())
}

/// `value` ticks of `from` in ticks of `to`, rounded down.
fn rescale(value: u64, from: u32, to: u32) -> u64 {
    (u128::from(value) * u128::from(to) / u128::from(from)) as u64
}

impl Mp4 {
    /// Start an MP4 at `path` with a video track, an audio track, or both.
    /// An audio-only file is an `.m4a` (brand `M4A `).
    pub fn create(path: &Path, video: Option<Video>, audio: Option<Audio>) -> io::Result<Self> {
        if video.is_none() && audio.is_none() {
            return Err(invalid("an MP4 needs a track"));
        }
        if let Some(v) = &video {
            if v.width > u32::from(u16::MAX) || v.height > u32::from(u16::MAX) {
                return Err(invalid("a picture too large for MP4"));
            }
            if v.sps.len() < 4 || v.pps.is_empty() {
                return Err(invalid("parameter sets too short to be H.264's"));
            }
        }
        if let Some(a) = &audio
            && (a.sample_rate == 0 || !(1..=2).contains(&a.channels) || a.config.is_empty())
        {
            return Err(invalid(
                "an audio track needs a rate, 1 or 2 channels and a config",
            ));
        }
        let mut out = BufWriter::new(File::create(path)?);
        let mut ftyp = Vec::new();
        let brands: [&[u8; 4]; 4] = if video.is_some() {
            put(&mut ftyp, b"isom");
            [b"isom", b"iso2", b"avc1", b"mp41"]
        } else {
            put(&mut ftyp, b"M4A ");
            [b"M4A ", b"isom", b"iso2", b"mp41"]
        };
        put(&mut ftyp, &0x200u32.to_be_bytes());
        for brand in brands {
            put(&mut ftyp, brand);
        }
        let ftyp = boxed(b"ftyp", &ftyp);
        debug_assert_eq!(ftyp.len() as u64 + 8, MDAT_SIZE_AT);
        out.write_all(&ftyp)?;
        // mdat, size 1 meaning "the 64-bit size follows", patched at the end.
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;

        let mut tracks = Vec::new();
        let video_index = video.map(|v| {
            tracks.push(Track::new(Kind::Video(v)));
            tracks.len() - 1
        });
        let audio_index = audio.map(|a| {
            tracks.push(Track::new(Kind::Audio(a)));
            tracks.len() - 1
        });
        Ok(Self {
            out,
            tracks,
            video: video_index,
            audio: audio_index,
            position: MDAT_SIZE_AT + 8,
            waiting_bytes: 0,
        })
    }

    /// Append a video frame that starts at `start` ticks of [`TIMESCALE`],
    /// as its NAL units (without start codes); `sync` for an IDR frame.
    /// Starts only go forward: a frame at or before the last one is moved
    /// to one tick after it, since a frame of no length is one players drop.
    pub fn push_video(&mut self, start: u64, nals: &[Vec<u8>], sync: bool) -> io::Result<()> {
        let index = self
            .video
            .ok_or_else(|| invalid("this MP4 has no video track"))?;
        let mut data = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
        for nal in nals {
            let len = u32::try_from(nal.len()).map_err(|_| invalid("a NAL unit over 4 GiB"))?;
            data.extend_from_slice(&len.to_be_bytes());
            data.extend_from_slice(nal);
        }
        let track = &mut self.tracks[index];
        let start = match track.last_start {
            Some(last) => start.max(last + 1),
            None => start,
        };
        track.last_start = Some(start);
        let time = start as f64 / f64::from(TIMESCALE);
        self.wait(index, data, start, sync, time)
    }

    /// Append one AAC access unit: a raw frame of [`AAC_FRAME_LEN`] samples
    /// per channel, as `raven_aac::Encoder` returns them.
    pub fn push_audio(&mut self, frame: &[u8]) -> io::Result<()> {
        let index = self
            .audio
            .ok_or_else(|| invalid("this MP4 has no audio track"))?;
        let track = &self.tracks[index];
        let Kind::Audio(audio) = &track.kind else {
            unreachable!("the audio index is an audio track")
        };
        let start = track.pushed * u64::from(AAC_FRAME_LEN);
        let time = (start as f64 - f64::from(audio.priming)) / f64::from(audio.sample_rate);
        self.wait(index, frame.to_vec(), start, true, time)
    }

    fn wait(
        &mut self,
        index: usize,
        data: Vec<u8>,
        start: u64,
        sync: bool,
        time: f64,
    ) -> io::Result<()> {
        let size = u32::try_from(data.len()).map_err(|_| invalid("a sample over 4 GiB"))?;
        let track = &mut self.tracks[index];
        track.pushed += 1;
        track.samples.push(Sample { size, start, sync });
        self.waiting_bytes += data.len();
        track.waiting.push_back(Waiting { data, time });
        self.pump(false)
    }

    /// Write whatever chunks are ready, earliest first. With `all`, every
    /// waiting sample is ready.
    fn pump(&mut self, all: bool) -> io::Result<()> {
        loop {
            let Some(index) = (0..self.tracks.len())
                .filter(|&i| !self.tracks[i].waiting.is_empty())
                .min_by(|&a, &b| {
                    let t = |i: usize| self.tracks[i].waiting[0].time;
                    t(a).total_cmp(&t(b))
                })
            else {
                return Ok(());
            };
            let waiting = &self.tracks[index].waiting;
            let until = waiting[0].time + CHUNK_SECONDS;
            let count = waiting.iter().take_while(|w| w.time < until).count().max(1);
            // A chunk is ready once a sample beyond it has arrived: until
            // then, more of it may still come.
            let ready = all || count < waiting.len() || self.waiting_bytes > MOST_PENDING;
            if !ready {
                return Ok(());
            }
            let track = &mut self.tracks[index];
            track.chunks.push((self.position, count as u32));
            for w in track.waiting.drain(..count) {
                self.out.write_all(&w.data)?;
                self.position += w.data.len() as u64;
                self.waiting_bytes -= w.data.len();
            }
        }
    }

    /// Finish the file at `end` ticks of [`TIMESCALE`]: where the last video
    /// frame stops, and where the sound is cut (the last AAC frame is padded
    /// past the real end). Returns the file's size.
    pub fn finish(mut self, end: u64) -> io::Result<u64> {
        if self.tracks.iter().any(|t| t.samples.is_empty()) {
            return Err(invalid("a track without samples"));
        }
        self.pump(true)?;
        let moov = self.moov(end);
        self.out.write_all(&moov)?;
        let total = self.position + moov.len() as u64;
        self.out.seek(SeekFrom::Start(MDAT_SIZE_AT))?;
        self.out
            .write_all(&(self.position - (MDAT_SIZE_AT - 8)).to_be_bytes())?;
        self.out.flush()?;
        Ok(total)
    }

    fn moov(&self, end: u64) -> Vec<u8> {
        let traks: Vec<(Vec<u8>, u64)> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| trak(t, i as u32 + 1, end))
            .collect();
        let movie_duration = traks.iter().map(|(_, d)| *d).max().unwrap_or(0);

        let mut mvhd = Vec::new();
        put(&mut mvhd, &[0; 8]); // creation, modification
        put(&mut mvhd, &MOVIE_TIMESCALE.to_be_bytes());
        put(&mut mvhd, &(movie_duration as u32).to_be_bytes());
        put(&mut mvhd, &0x0001_0000u32.to_be_bytes()); // rate 1.0
        put(&mut mvhd, &0x0100u16.to_be_bytes()); // volume 1.0
        put(&mut mvhd, &[0; 10]);
        for m in MATRIX {
            put(&mut mvhd, &m.to_be_bytes());
        }
        put(&mut mvhd, &[0; 24]); // pre_defined
        put(&mut mvhd, &(self.tracks.len() as u32 + 1).to_be_bytes()); // next_track_ID
        let mut body = full(b"mvhd", 0, 0, &mvhd);
        for (trak, _) in traks {
            body.extend(trak);
        }
        boxed(b"moov", &body)
    }
}

/// A track's `trak` box, and its duration in the movie's timescale.
fn trak(track: &Track, id: u32, end: u64) -> (Vec<u8>, u64) {
    let scale = track.timescale();
    let is_video = matches!(track.kind, Kind::Video(_));
    // Media duration: to `end` for video; every frame's samples for audio.
    let (media_end, shown, priming) = match &track.kind {
        Kind::Video(_) => {
            let last = track.samples.last().map_or(0, |s| s.start);
            let end = end.max(last + 1);
            (end, end, 0)
        }
        Kind::Audio(a) => {
            let media = track.samples.len() as u64 * u64::from(AAC_FRAME_LEN);
            let real = media.saturating_sub(u64::from(a.priming));
            let cut = rescale(end, TIMESCALE, a.sample_rate);
            (media, real.min(cut), a.priming)
        }
    };
    let duration = rescale(shown, scale, MOVIE_TIMESCALE);

    let mut tkhd = Vec::new();
    put(&mut tkhd, &[0; 8]);
    put(&mut tkhd, &id.to_be_bytes());
    put(&mut tkhd, &[0; 4]);
    put(&mut tkhd, &(duration as u32).to_be_bytes());
    put(&mut tkhd, &[0; 8]);
    put(&mut tkhd, &[0; 4]); // layer, alternate_group
    put(
        &mut tkhd,
        &(if is_video { 0u16 } else { 0x0100 }).to_be_bytes(),
    ); // volume
    put(&mut tkhd, &[0; 2]);
    for m in MATRIX {
        put(&mut tkhd, &m.to_be_bytes());
    }
    let (w, h) = match &track.kind {
        Kind::Video(v) => (v.width, v.height),
        Kind::Audio(_) => (0, 0),
    };
    put(&mut tkhd, &(w << 16).to_be_bytes());
    put(&mut tkhd, &(h << 16).to_be_bytes());
    let tkhd = full(b"tkhd", 0, 3, &tkhd); // enabled, in movie

    // The edit list: the audio's priming is media the movie never shows.
    let edts = if is_video {
        Vec::new()
    } else {
        let mut elst = 1u32.to_be_bytes().to_vec();
        put(&mut elst, &(duration as u32).to_be_bytes());
        put(&mut elst, &priming.to_be_bytes());
        put(&mut elst, &0x0001_0000u32.to_be_bytes()); // rate 1.0
        boxed(b"edts", &full(b"elst", 0, 0, &elst))
    };

    let mut mdhd = Vec::new();
    put(&mut mdhd, &[0; 16]); // creation, modification (64-bit)
    put(&mut mdhd, &scale.to_be_bytes());
    put(&mut mdhd, &media_end.to_be_bytes());
    put(&mut mdhd, &0x55C4u16.to_be_bytes()); // language "und"
    put(&mut mdhd, &[0; 2]);
    let mdhd = full(b"mdhd", 1, 0, &mdhd);

    let mut hdlr = Vec::new();
    put(&mut hdlr, &[0; 4]);
    put(&mut hdlr, if is_video { b"vide" } else { b"soun" });
    put(&mut hdlr, &[0; 12]);
    put(
        &mut hdlr,
        if is_video {
            b"Raven video\0" as &[u8]
        } else {
            b"Raven audio\0"
        },
    );
    let hdlr = full(b"hdlr", 0, 0, &hdlr);

    let header = if is_video {
        full(b"vmhd", 0, 1, &[0; 8])
    } else {
        full(b"smhd", 0, 0, &[0; 4]) // balance, reserved
    };
    let dref = full(
        b"dref",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat(),
    );
    let dinf = boxed(b"dinf", &dref);

    let mut stbl = match &track.kind {
        Kind::Video(v) => video_stsd(v),
        Kind::Audio(a) => audio_stsd(a, track),
    };
    stbl.extend(stts(track, media_end));
    if is_video {
        stbl.extend(stss(track));
    }
    stbl.extend(stsc(track));
    stbl.extend(stsz(track));
    stbl.extend(chunk_offsets(track));
    let stbl = boxed(b"stbl", &stbl);

    let minf = boxed(b"minf", &[header, dinf, stbl].concat());
    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
    (boxed(b"trak", &[tkhd, edts, mdia].concat()), duration)
}

fn video_stsd(v: &Video) -> Vec<u8> {
    let mut avcc = vec![1, v.sps[1], v.sps[2], v.sps[3], 0xFF, 0xE1];
    put(&mut avcc, &(v.sps.len() as u16).to_be_bytes());
    put(&mut avcc, &v.sps);
    avcc.push(1);
    put(&mut avcc, &(v.pps.len() as u16).to_be_bytes());
    put(&mut avcc, &v.pps);

    // nclx: BT.709 primaries, transfer and matrix, limited range — the
    // same as raven-h264's SPS says, for players that read only the container.
    let mut colr = Vec::new();
    put(&mut colr, b"nclx");
    for value in [1u16, 1, 1] {
        put(&mut colr, &value.to_be_bytes());
    }
    colr.push(0);

    let mut entry = Vec::new();
    put(&mut entry, &[0; 6]);
    put(&mut entry, &1u16.to_be_bytes()); // data_reference_index
    put(&mut entry, &[0; 16]);
    put(&mut entry, &(v.width as u16).to_be_bytes());
    put(&mut entry, &(v.height as u16).to_be_bytes());
    put(&mut entry, &0x0048_0000u32.to_be_bytes()); // 72 dpi
    put(&mut entry, &0x0048_0000u32.to_be_bytes());
    put(&mut entry, &[0; 4]);
    put(&mut entry, &1u16.to_be_bytes()); // frame_count
    let mut name = [0u8; 32];
    let label = b"Raven H.264";
    name[0] = label.len() as u8;
    name[1..=label.len()].copy_from_slice(label);
    put(&mut entry, &name);
    put(&mut entry, &0x0018u16.to_be_bytes()); // depth
    put(&mut entry, &(-1i16).to_be_bytes());
    put(&mut entry, &boxed(b"avcC", &avcc));
    put(&mut entry, &boxed(b"colr", &colr));

    full(
        b"stsd",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &boxed(b"avc1", &entry)].concat(),
    )
}

fn audio_stsd(a: &Audio, track: &Track) -> Vec<u8> {
    // The bitrates esds declares: the average, and the most any one second
    // of frames carries.
    let sizes: Vec<u64> = track.samples.iter().map(|s| u64::from(s.size)).collect();
    let per_second = (a.sample_rate / AAC_FRAME_LEN).max(1) as usize;
    let seconds =
        (sizes.len() as f64 * f64::from(AAC_FRAME_LEN) / f64::from(a.sample_rate)).max(1e-3);
    let average = (sizes.iter().sum::<u64>() as f64 * 8.0 / seconds) as u32;
    let mut window: u64 = sizes.iter().take(per_second).sum();
    let mut most = window;
    for i in per_second..sizes.len() {
        window = window + sizes[i] - sizes[i - per_second];
        most = most.max(window);
    }
    let peak = u32::try_from(most * 8).unwrap_or(u32::MAX).max(average);
    let buffer = sizes.iter().copied().max().unwrap_or(0) as u32;

    let mut config = vec![0x40, 0x15]; // MPEG-4 audio; audio stream, upstream 0, reserved 1
    put(&mut config, &buffer.to_be_bytes()[1..]); // bufferSizeDB, 24 bits
    put(&mut config, &peak.to_be_bytes());
    put(&mut config, &average.to_be_bytes());
    put(&mut config, &descriptor(5, &a.config));
    let mut es = vec![0, 0, 0]; // ES_ID, no dependence, URL or OCR stream
    put(&mut es, &descriptor(4, &config));
    put(&mut es, &descriptor(6, &[2])); // SLConfig: predefined for MP4
    let esds = full(b"esds", 0, 0, &descriptor(3, &es));

    let mut entry = Vec::new();
    put(&mut entry, &[0; 6]);
    put(&mut entry, &1u16.to_be_bytes()); // data_reference_index
    put(&mut entry, &[0; 8]);
    put(&mut entry, &u16::from(a.channels).to_be_bytes());
    put(&mut entry, &16u16.to_be_bytes()); // samplesize
    put(&mut entry, &[0; 4]); // pre_defined, reserved
    // samplerate, 16.16; a rate that does not fit is left to the esds.
    let rate = u16::try_from(a.sample_rate).unwrap_or(0);
    put(&mut entry, &(u32::from(rate) << 16).to_be_bytes());
    put(&mut entry, &esds);
    full(
        b"stsd",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &boxed(b"mp4a", &entry)].concat(),
    )
}

/// Durations, run-length coded: a recording at a steady 30 Hz is one entry.
fn stts(track: &Track, end: u64) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (i, sample) in track.samples.iter().enumerate() {
        let next = track.samples.get(i + 1).map_or(end, |s| s.start);
        let delta = u32::try_from(next.saturating_sub(sample.start)).unwrap_or(u32::MAX);
        match runs.last_mut() {
            Some((count, d)) if *d == delta => *count += 1,
            _ => runs.push((1, delta)),
        }
    }
    let mut body = (runs.len() as u32).to_be_bytes().to_vec();
    for (count, delta) in runs {
        put(&mut body, &count.to_be_bytes());
        put(&mut body, &delta.to_be_bytes());
    }
    full(b"stts", 0, 0, &body)
}

fn stss(track: &Track) -> Vec<u8> {
    let sync: Vec<u32> = (1..)
        .zip(&track.samples)
        .filter(|(_, s)| s.sync)
        .map(|(n, _)| n)
        .collect();
    let mut body = (sync.len() as u32).to_be_bytes().to_vec();
    for n in sync {
        put(&mut body, &n.to_be_bytes());
    }
    full(b"stss", 0, 0, &body)
}

/// Samples per chunk, as runs: an entry wherever the count changes.
fn stsc(track: &Track) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (i, &(_, count)) in track.chunks.iter().enumerate() {
        if runs.last().is_none_or(|&(_, c)| c != count) {
            runs.push((i as u32 + 1, count));
        }
    }
    let mut body = (runs.len() as u32).to_be_bytes().to_vec();
    for (first, count) in runs {
        for v in [first, count, 1] {
            put(&mut body, &v.to_be_bytes());
        }
    }
    full(b"stsc", 0, 0, &body)
}

fn stsz(track: &Track) -> Vec<u8> {
    let mut body = [0u32, track.samples.len() as u32]
        .map(u32::to_be_bytes)
        .concat();
    for sample in &track.samples {
        put(&mut body, &sample.size.to_be_bytes());
    }
    full(b"stsz", 0, 0, &body)
}

/// `stco`, or `co64` once the file is past 4 GiB.
fn chunk_offsets(track: &Track) -> Vec<u8> {
    let mut body = (track.chunks.len() as u32).to_be_bytes().to_vec();
    let wide = track.chunks.iter().any(|&(o, _)| o > u64::from(u32::MAX));
    for &(offset, _) in &track.chunks {
        if wide {
            put(&mut body, &offset.to_be_bytes());
        } else {
            put(&mut body, &(offset as u32).to_be_bytes());
        }
    }
    full(if wide { b"co64" } else { b"stco" }, 0, 0, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_sizes_use_seven_bit_groups() {
        assert_eq!(descriptor(5, &[1, 2]), [5, 2, 1, 2]);
        let long = descriptor(4, &[0; 200]);
        assert_eq!(&long[..3], &[4, 0x81, 0x48]);
    }

    #[test]
    fn a_file_needs_a_track_and_a_track_needs_samples() {
        let path = std::env::temp_dir().join(format!("raven-mp4-empty-{}.mp4", std::process::id()));
        assert!(Mp4::create(&path, None, None).is_err());
        let audio = Audio {
            sample_rate: 48_000,
            channels: 2,
            config: vec![0x11, 0x90],
            priming: 1024,
        };
        let mut mp4 = Mp4::create(&path, None, Some(audio)).unwrap();
        assert!(mp4.push_video(0, &[vec![0x65]], true).is_err());
        assert!(mp4.finish(90_000).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
