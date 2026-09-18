//! Raven's own AAC encoder.
//!
//! MPEG-4 AAC Low Complexity (ISO/IEC 14496-3, the same coding as ISO/IEC
//! 13818-7), the audio every MP4 player plays: what Raven Camera puts next to
//! `raven-h264`'s video, and alone in `.m4a` files. Nothing outside the tree
//! is involved, so nothing outside the tree can break it.
//!
//! The encoder is a whole one, not the least that decodes:
//!
//! - **Filterbank.** A sine-windowed MDCT, 1024 lines a frame, computed
//!   through an FFT (`mdct.rs`). Where a frame holds a sudden attack it is
//!   coded as eight short blocks of 128 lines instead, with the start and
//!   stop windows either side, so that quantisation noise cannot spread
//!   ahead of the attack as a pre-echo.
//! - **Psychoacoustics.** A masking threshold per band, from band energies
//!   spread over the Bark scale, a tonality-dependent signal-to-mask ratio
//!   and the threshold of hearing (`psy.rs`).
//! - **Rate control.** One offset scales every band's noise allowance until
//!   the frame fits the bits it is given; a bit reservoir lets hard frames
//!   borrow from easy ones, never beyond the standard's 6144 bits per channel.
//! - **Entropy coding.** Sections and Huffman books chosen by dynamic
//!   programming over all eleven spectral books, escapes included, and
//!   Huffman-coded scalefactors (`quant.rs`).
//! - **Stereo.** Mid/side per band where it saves bits.
//!
//! What it leaves out are the tools that buy little at these rates and that
//! some decoders get wrong: temporal noise shaping, intensity stereo,
//! perceptual noise substitution and the Kaiser-Bessel window.
//!
//! # Use
//!
//! ```
//! let mut encoder = raven_aac::Encoder::new(raven_aac::Config::new(48_000, 2)).unwrap();
//! let asc = encoder.audio_specific_config(); // for the MP4's esds box
//! let mut frames = encoder.encode(&vec![0i16; 2 * 4800]); // any amount, interleaved
//! frames.extend(encoder.finish());
//! assert_eq!(asc.len(), 2);
//! assert_eq!(frames.len(), 6); // 4800 samples, plus the 1024 of priming
//! ```
//!
//! Every frame decodes to 1024 samples per channel, and the first
//! [`PRIMING`] samples a decoder produces come before the input starts: an
//! MP4 skips them with an edit list.

mod bits;
mod huffman;
mod mdct;
mod psy;
mod quant;
mod syntax;
mod tables;

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;

use mdct::{Mdct, sine_half};
use psy::{BandModel, Channel, Detector, MAX_BANDS};

/// Samples per channel in every frame.
pub const FRAME_LEN: usize = 1024;

/// Samples per channel a decoder produces before the first input sample:
/// the encoder delay, which an MP4 edit list skips.
pub const PRIMING: u32 = 1024;

/// Bits one frame may take per channel, the decoder's input buffer.
const MAX_BITS_PER_CHANNEL: usize = 6144;

/// What to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Samples a second: one of the rates AAC names, 8000 to 96000.
    pub sample_rate: u32,
    /// 1 or 2; two are interleaved left, right.
    pub channels: u8,
    /// The average bitrate to aim for, in bits a second.
    pub bitrate: u32,
}

impl Config {
    /// `channels` at `sample_rate`, at 64 kb/s a channel: 128 kb/s stereo,
    /// where AAC-LC is transparent to most ears.
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            sample_rate,
            channels,
            bitrate: 64_000 * u32::from(channels),
        }
    }
}

/// Everything that can stop an encoder being made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Not one of the rates AAC can signal.
    SampleRate(u32),
    /// Not 1 or 2.
    Channels(u8),
    /// Below 8 kb/s a channel, or more than a frame can hold.
    Bitrate(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleRate(rate) => write!(f, "AAC cannot carry {rate} Hz audio"),
            Self::Channels(n) => write!(f, "{n} channels (1 or 2 are supported)"),
            Self::Bitrate(rate) => write!(f, "{rate} bit/s is outside what AAC can do here"),
        }
    }
}

impl std::error::Error for Error {}

/// `window_sequence`, as the bitstream numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sequence {
    OnlyLong = 0,
    LongStart = 1,
    EightShort = 2,
    LongStop = 3,
}

/// How one frame's 1024 lines are laid out in scalefactor bands.
///
/// For short blocks, windows are grouped and a group's windows share
/// scalefactors: the lines of band `sfb` of every window in the group are
/// consecutive, the order spectral data is coded in. Bands are numbered
/// flat, `g * swb + sfb`, and each is one contiguous range of the frame.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub(crate) sequence: Sequence,
    pub(crate) short: bool,
    pub(crate) groups: usize,
    pub(crate) group_len: [u8; 8],
    /// Bands in a group.
    pub(crate) swb: usize,
    /// Bands in a group below the cut-off frequency.
    pub(crate) coded: usize,
    edges: Vec<u16>,
}

impl Layout {
    pub(crate) fn bands(&self) -> usize {
        self.groups * self.swb
    }

    pub(crate) fn band(&self, b: usize) -> Range<usize> {
        usize::from(self.edges[b])..usize::from(self.edges[b + 1])
    }

    pub(crate) fn sfb(&self, b: usize) -> usize {
        b % self.swb
    }
}

/// The encoder: interleaved 16-bit samples in, AAC frames out.
///
/// Each frame is a raw `raw_data_block()`, what an MP4 sample holds; prefix
/// [`adts_header`] to make an `.aac` stream instead.
#[derive(Debug)]
pub struct Encoder {
    config: Config,
    rate_index: usize,
    long_edges: &'static [u16],
    short_edges: &'static [u16],
    long_model: BandModel,
    short_model: BandModel,
    /// Bands, per group, below the cut-off frequency.
    long_coded: usize,
    short_coded: usize,
    /// Samples per channel, the first being sample `start` of the stream as
    /// the encoder sees it: `PRIMING` zeros, then the input.
    queue: Vec<Vec<f32>>,
    start: usize,
    /// Half a stereo sample, carried until its other half arrives.
    partial: Option<i16>,
    received: u64,
    /// The next frame to encode.
    frame: usize,
    /// Whether it holds an attack, once known.
    attack: Option<bool>,
    /// Which 128-sample segments hold attacks, from `first_segment` on;
    /// segment `s` starts at stream sample `448 + 128 s`.
    segments: VecDeque<bool>,
    first_segment: usize,
    previous: Sequence,
    detectors: Vec<Detector>,
    long_mdct: Mdct,
    short_mdct: Mdct,
    /// The three long windows, whole: only-long, start and stop.
    long_windows: [Vec<f32>; 3],
    /// A short window, whole.
    short_window: Vec<f32>,
    scratch: Vec<f32>,
    raw: Vec<Vec<f32>>,
    channels: Vec<Channel>,
    ms: Vec<bool>,
    /// Bits a frame gets on average.
    mean_bits: f64,
    /// Bits saved by earlier frames, which later ones may spend.
    reservoir: f64,
    /// The perceptual entropy of recent frames, for comparison.
    entropy: f64,
    /// The last frame's quantiser offset, where the next search starts.
    offset: i32,
}

/// The highest frequency worth coding at `bits_per_channel` a second.
///
/// Every line above it would take bits from the lines below, which are the
/// ones heard; at 64 kb/s a channel the cut is near 16 kHz, where most adult
/// hearing ends anyway.
fn cutoff(bits_per_channel: u32, sample_rate: u32) -> f32 {
    let hz = 3_000.0 + 0.2 * bits_per_channel as f32;
    hz.min(20_000.0).min(sample_rate as f32 / 2.0)
}

impl Encoder {
    /// An encoder for `config`.
    pub fn new(config: Config) -> Result<Self, Error> {
        let rate_index = tables::SAMPLE_RATES
            .iter()
            .position(|&r| r == config.sample_rate)
            .ok_or(Error::SampleRate(config.sample_rate))?;
        if !(1..=2).contains(&config.channels) {
            return Err(Error::Channels(config.channels));
        }
        let channels = usize::from(config.channels);
        let per_channel = config.bitrate / u32::from(config.channels);
        let most = (MAX_BITS_PER_CHANNEL as u64 * u64::from(config.sample_rate) / 1024) as u32;
        if per_channel < 8_000 || per_channel > most {
            return Err(Error::Bitrate(config.bitrate));
        }
        let (long_edges, short_edges) = tables::bands(rate_index);
        let top = cutoff(per_channel, config.sample_rate);
        let coded = |edges: &[u16], lines: usize| {
            let line = top * (2 * lines) as f32 / config.sample_rate as f32;
            edges[..edges.len() - 1]
                .iter()
                .take_while(|&&e| f32::from(e) < line)
                .count()
        };
        let rise_long = sine_half(1024);
        let rise_short = sine_half(128);
        let long_window = |kind: Sequence| -> Vec<f32> {
            (0..2048)
                .map(|n| match kind {
                    Sequence::LongStart if n >= 1024 => match n {
                        1024..1472 => 1.0,
                        1472..1600 => rise_short[1599 - n],
                        _ => 0.0,
                    },
                    Sequence::LongStop if n < 1024 => match n {
                        0..448 => 0.0,
                        448..576 => rise_short[n - 448],
                        _ => 1.0,
                    },
                    _ if n < 1024 => rise_long[n],
                    _ => rise_long[2047 - n],
                })
                .collect()
        };
        let mut queue = vec![Vec::with_capacity(8192); channels];
        for q in &mut queue {
            q.resize(PRIMING as usize, 0.0);
        }
        Ok(Self {
            config,
            rate_index,
            long_edges,
            short_edges,
            long_model: BandModel::new(long_edges, 1024, config.sample_rate),
            short_model: BandModel::new(short_edges, 128, config.sample_rate),
            long_coded: coded(long_edges, 1024),
            short_coded: coded(short_edges, 128),
            queue,
            start: 0,
            partial: None,
            received: 0,
            frame: 0,
            attack: None,
            segments: VecDeque::new(),
            first_segment: 0,
            previous: Sequence::OnlyLong,
            detectors: vec![Detector::default(); channels],
            long_mdct: Mdct::new(1024),
            short_mdct: Mdct::new(128),
            long_windows: [
                long_window(Sequence::OnlyLong),
                long_window(Sequence::LongStart),
                long_window(Sequence::LongStop),
            ],
            short_window: (0..256)
                .map(|n| rise_short[if n < 128 { n } else { 255 - n }])
                .collect(),
            scratch: vec![0.0; 2048],
            raw: vec![vec![0.0; 1024]; channels],
            channels: vec![Channel::new(); channels],
            ms: vec![false; MAX_BANDS],
            mean_bits: f64::from(config.bitrate) * 1024.0 / f64::from(config.sample_rate),
            reservoir: 0.0,
            entropy: 0.0,
            offset: 0,
        }
        .full())
    }

    /// A decoder fills its input buffer before it starts, so the first
    /// frame may already draw on a full reservoir.
    fn full(mut self) -> Self {
        self.reservoir = self.max_reservoir();
        self
    }

    /// What this encoder was made for.
    pub fn config(&self) -> Config {
        self.config
    }

    /// The `AudioSpecificConfig` (ISO/IEC 14496-3, 1.6.2.1) describing the
    /// stream: what an MP4's `esds` box carries as its decoder-specific info.
    pub fn audio_specific_config(&self) -> Vec<u8> {
        audio_specific_config(self.rate_index as u8, self.config.channels)
    }

    /// Samples per channel taken in so far.
    pub fn samples(&self) -> u64 {
        self.received
    }

    /// Encode interleaved samples, any number of them. Returns the frames
    /// completed, which may be none: frames are 1024 samples, and the
    /// encoder looks one frame ahead to see attacks coming.
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<Vec<u8>> {
        let mut pcm = pcm;
        if let (Some(left), Some((&right, rest))) = (self.partial, pcm.split_first()) {
            self.push(&[left, right]);
            self.partial = None;
            pcm = rest;
        }
        let whole = pcm.len() / self.queue.len() * self.queue.len();
        self.push(&pcm[..whole]);
        if whole < pcm.len() {
            self.partial = Some(pcm[whole]);
        }
        let mut frames = Vec::new();
        while self.available() >= self.frame * FRAME_LEN + LOOKAHEAD {
            frames.push(self.encode_frame());
        }
        frames
    }

    /// Encode what is left, padded with silence to whole frames. The last
    /// frame carries the last input sample; a player trims the padding by
    /// the MP4's edit list (or, for raw streams, not at all).
    pub fn finish(mut self) -> Vec<Vec<u8>> {
        if self.received == 0 {
            return Vec::new();
        }
        let frames = self.received.div_ceil(FRAME_LEN as u64) as usize + 1;
        let needed = (frames - 1) * FRAME_LEN + LOOKAHEAD;
        let pad = needed.saturating_sub(self.available());
        for q in &mut self.queue {
            q.resize(q.len() + pad, 0.0);
        }
        let mut out = Vec::new();
        while self.frame < frames {
            out.push(self.encode_frame());
        }
        out
    }

    fn push(&mut self, pcm: &[i16]) {
        let n = self.queue.len();
        for (c, q) in self.queue.iter_mut().enumerate() {
            q.extend(pcm.iter().skip(c).step_by(n).map(|&s| f32::from(s)));
        }
        self.received += (pcm.len() / n) as u64;
    }

    fn available(&self) -> usize {
        self.start + self.queue[0].len()
    }

    /// Whether frame `t` has an attack anywhere its short windows reach.
    ///
    /// The span is samples 448 to 1600 of the frame's block: 128 more than
    /// a frame's worth, so that an attack near either end is claimed by the
    /// neighbouring frame too. Were it not, the neighbour's start or stop
    /// window would still take in the attack on its slope, and spread its
    /// noise back over a whole long block.
    fn detect(&mut self, t: usize) -> bool {
        let last = 8 * t + 8;
        while self.segments.len() + self.first_segment <= last {
            let s = self.segments.len() + self.first_segment;
            let at = 448 + 128 * s - self.start;
            let mut found = false;
            for (d, q) in self.detectors.iter_mut().zip(&self.queue) {
                found |= d.attack(q[at - 1], &q[at..at + 128]);
            }
            self.segments.push_back(found);
        }
        while self.first_segment < 8 * t {
            self.segments.pop_front();
            self.first_segment += 1;
        }
        self.segments.iter().take(9).any(|&a| a)
    }

    fn encode_frame(&mut self) -> Vec<u8> {
        let t = self.frame;
        // Frame 0's attack region straddles the start of the input, and
        // anything ahead of the input is trimmed away by the edit list, so
        // there a pre-echo is never heard.
        let attack = match self.attack {
            Some(a) => a,
            None => {
                self.detect(t);
                false
            }
        };
        let next = self.detect(t + 1);
        self.attack = Some(next);
        // Short blocks are entered through a start window and left through
        // a stop window; a frame that sees an attack coming in the next one
        // becomes the start. Short then short again needs no stop between.
        let sequence = match self.previous {
            Sequence::LongStart => Sequence::EightShort,
            Sequence::EightShort if attack || next => Sequence::EightShort,
            Sequence::EightShort => Sequence::LongStop,
            _ if next => Sequence::LongStart,
            _ => Sequence::OnlyLong,
        };
        debug_assert!(!attack || sequence == Sequence::EightShort);

        self.transform(t, sequence);
        let layout = self.layout(sequence);
        for (ch, raw) in self.channels.iter_mut().zip(&self.raw) {
            regroup(raw, &mut ch.spec, &layout, self.short_edges);
            let model = if layout.short {
                &self.short_model
            } else {
                &self.long_model
            };
            ch.mask(&layout, model);
        }
        if self.channels.len() == 2 {
            psy::mid_side(&mut self.channels, &layout, &mut self.ms);
        } else {
            self.ms.fill(false);
        }
        let mut entropy = 0.0;
        for ch in &mut self.channels {
            ch.prepare(&layout);
            entropy += f64::from(ch.entropy(&layout));
        }

        let budget = self.budget(entropy, layout.short);
        let offset = self.search(&layout, budget);
        let (bits, max_sfb) = self.plan(&layout, offset);
        let out = syntax::frame(&self.channels, &layout, &self.ms, max_sfb, bits / 8);
        debug_assert_eq!(out.len() * 8, bits, "counted and written frames differ");

        let most = self.max_reservoir();
        self.reservoir = (self.reservoir + self.mean_bits - bits as f64).clamp(0.0, most);
        self.previous = sequence;
        self.frame += 1;
        // Samples before the next frame's block are no longer needed.
        let keep_from = self.frame * FRAME_LEN;
        let drop = keep_from - self.start;
        for q in &mut self.queue {
            q.drain(..drop);
        }
        self.start = keep_from;
        out
    }

    /// Window and transform frame `t` of every channel into `raw`.
    fn transform(&mut self, t: usize, sequence: Sequence) {
        let at = t * FRAME_LEN - self.start;
        for (q, raw) in self.queue.iter().zip(&mut self.raw) {
            let block = &q[at..at + 2048];
            if sequence == Sequence::EightShort {
                for w in 0..8 {
                    let part = &block[448 + 128 * w..448 + 128 * w + 256];
                    for ((s, x), win) in self.scratch.iter_mut().zip(part).zip(&self.short_window) {
                        *s = x * win;
                    }
                    self.short_mdct
                        .forward(&self.scratch[..256], &mut raw[128 * w..128 * (w + 1)]);
                }
            } else {
                let window = match sequence {
                    Sequence::LongStart => &self.long_windows[1],
                    Sequence::LongStop => &self.long_windows[2],
                    _ => &self.long_windows[0],
                };
                for ((s, x), win) in self.scratch.iter_mut().zip(block).zip(window) {
                    *s = x * win;
                }
                self.long_mdct.forward(&self.scratch, raw);
            }
        }
    }

    /// The frame's band layout; for short blocks, windows are grouped where
    /// their energy is alike, so that they can share scalefactors.
    fn layout(&self, sequence: Sequence) -> Layout {
        if sequence != Sequence::EightShort {
            let swb = self.long_edges.len() - 1;
            return Layout {
                sequence,
                short: false,
                groups: 1,
                group_len: [1, 0, 0, 0, 0, 0, 0, 0],
                swb,
                coded: self.long_coded,
                edges: self.long_edges.to_vec(),
            };
        }
        let top = usize::from(self.short_edges[self.short_coded]);
        let energy: Vec<f32> = (0..8)
            .map(|w| {
                self.raw
                    .iter()
                    .map(|r| r[128 * w..128 * w + top].iter().map(|x| x * x).sum::<f32>())
                    .sum::<f32>()
                    + 1e6
            })
            .collect();
        let mut group_len = [0u8; 8];
        let mut groups = 0;
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        for (w, &e) in energy.iter().enumerate() {
            // Within 6 dB of every window already in the group: join it.
            if w > 0 && e <= lo * 4.0 && e * 4.0 >= hi {
                group_len[groups - 1] += 1;
                lo = lo.min(e);
                hi = hi.max(e);
            } else {
                group_len[groups] = 1;
                groups += 1;
                (lo, hi) = (e, e);
            }
        }
        let swb = self.short_edges.len() - 1;
        let mut edges = Vec::with_capacity(groups * swb + 1);
        let mut at = 0u16;
        for &len in &group_len[..groups] {
            for pair in self.short_edges.windows(2) {
                edges.push(at);
                at += (pair[1] - pair[0]) * u16::from(len);
            }
        }
        edges.push(at);
        debug_assert_eq!(at, 1024);
        Layout {
            sequence,
            short: true,
            groups,
            group_len,
            swb,
            coded: self.short_coded,
            edges,
        }
    }

    fn max_frame_bits(&self) -> usize {
        MAX_BITS_PER_CHANNEL * self.channels.len()
    }

    fn max_reservoir(&self) -> f64 {
        self.max_frame_bits() as f64 - self.mean_bits
    }

    /// The bits this frame should aim for: the average, more for frames
    /// harder than recent ones and less for easier, plus a share of what the
    /// reservoir has saved — never more than it holds.
    fn budget(&mut self, entropy: f64, short: bool) -> usize {
        if self.entropy <= 0.0 {
            // Before there is a history, compare with a modest frame, so
            // that an opening attack is not starved.
            self.entropy = 0.3 * self.mean_bits;
        }
        let most_demand = if short { 4.0 } else { 2.5 };
        let demand = ((entropy + 100.0) / (self.entropy + 100.0)).clamp(0.5, most_demand);
        self.entropy = 0.9 * self.entropy + 0.1 * entropy;
        // The reservoir is steered towards half full: room to save into
        // when frames are easy, and bits to hand out when an attack comes.
        let steer = 0.15 * (self.reservoir - self.max_reservoir() / 2.0);
        let target = self.mean_bits * demand.powf(0.6) + steer;
        let most = (self.mean_bits + self.reservoir).min(self.max_frame_bits() as f64);
        target.clamp(0.4 * self.mean_bits, most) as usize
    }

    /// Quantise and code every channel at `offset`. Returns the frame's
    /// bits and its `max_sfb`.
    fn plan(&mut self, layout: &Layout, offset: i32) -> (usize, usize) {
        let mut max_sfb = 0;
        for ch in &mut self.channels {
            quant::quantise(ch, layout, offset);
            max_sfb = max_sfb.max(ch.max_sfb);
        }
        let mut coded = [0usize; 2];
        for (ch, bits) in self.channels.iter_mut().zip(&mut coded) {
            *bits = quant::code(ch, layout, max_sfb);
        }
        let coded = &coded[..self.channels.len()];
        let mode = syntax::ms_mode(layout, &self.ms, max_sfb);
        (syntax::frame_bits(layout, coded, mode, max_sfb), max_sfb)
    }

    /// The smallest offset — the finest quantisation — whose frame fits
    /// `budget` bits. Bits only fall as the offset rises, so this brackets
    /// the answer outwards from the last frame's, which is usually close,
    /// and then bisects.
    fn search(&mut self, layout: &Layout, budget: usize) -> i32 {
        const LOWEST: i32 = -100;
        const HIGHEST: i32 = 255; // every band silent: always fits
        let guess = self.offset.clamp(LOWEST, HIGHEST);
        let mut fits = |g: i32| g >= HIGHEST || self.plan(layout, g).0 <= budget;
        // Invariant: `hi` fits; `lo` does not, or is below the range.
        let (mut lo, mut hi);
        let mut step = 2;
        if fits(guess) {
            hi = guess;
            lo = guess - step;
            while lo >= LOWEST && fits(lo) {
                hi = lo;
                step *= 2;
                lo = hi - step;
            }
            lo = lo.max(LOWEST - 1);
        } else {
            lo = guess;
            hi = (guess + step).min(HIGHEST);
            while !fits(hi) {
                lo = hi;
                step *= 2;
                hi = (lo + step).min(HIGHEST);
            }
        }
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if fits(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        self.offset = hi;
        hi
    }
}

/// What the encoder must have queued, past a frame's start, to encode it:
/// its own block and the attack span of the frame after.
const LOOKAHEAD: usize = FRAME_LEN + 1600;

/// Put a frame's lines into coding order (see [`Layout`]).
fn regroup(raw: &[f32], spec: &mut [f32], layout: &Layout, short_edges: &[u16]) {
    if !layout.short {
        spec.copy_from_slice(raw);
        return;
    }
    let mut at = 0;
    let mut window = 0;
    for &len in &layout.group_len[..layout.groups] {
        let len = usize::from(len);
        for pair in short_edges.windows(2) {
            let (lo, hi) = (usize::from(pair[0]), usize::from(pair[1]));
            for w in window..window + len {
                spec[at..at + hi - lo].copy_from_slice(&raw[128 * w + lo..128 * w + hi]);
                at += hi - lo;
            }
        }
        window += len;
    }
}

fn audio_specific_config(rate_index: u8, channels: u8) -> Vec<u8> {
    // audioObjectType 2 (LC), samplingFrequencyIndex, channelConfiguration,
    // then GASpecificConfig: 1024-sample frames, no core coder, no extension.
    const LC: u8 = 2;
    vec![
        (LC << 3) | (rate_index >> 1),
        ((rate_index & 1) << 7) | (channels << 3),
    ]
}

/// The 7-byte ADTS header (ISO/IEC 14496-3, 1.A.2) for a frame of
/// `payload` bytes, making a self-describing `.aac` stream: no CRC, a
/// variable bitrate, one raw data block.
pub fn adts_header(config: &Config, payload: usize) -> [u8; 7] {
    let rate = tables::SAMPLE_RATES
        .iter()
        .position(|&r| r == config.sample_rate)
        .unwrap_or(15) as u8;
    let len = (payload + 7).min(0x1FFF) as u32;
    let ch = config.channels;
    [
        0xFF,
        0xF1,                               // sync, MPEG-4, layer 0, no CRC
        (1 << 6) | (rate << 2) | (ch >> 2), // profile LC (object type - 1)
        ((ch & 3) << 6) | (len >> 11) as u8,
        (len >> 3) as u8,
        ((len & 7) << 5) as u8 | 0x1F, // buffer fullness 0x7FF: variable rate
        0xFC,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_specific_config_names_lc_the_rate_and_the_channels() {
        let e = Encoder::new(Config::new(48_000, 2)).unwrap();
        assert_eq!(e.audio_specific_config(), [0x11, 0x90]);
        let e = Encoder::new(Config::new(44_100, 1)).unwrap();
        assert_eq!(e.audio_specific_config(), [0x12, 0x08]);
    }

    #[test]
    fn configs_are_checked() {
        assert_eq!(
            Encoder::new(Config::new(44_000, 2)).unwrap_err(),
            Error::SampleRate(44_000)
        );
        assert_eq!(
            Encoder::new(Config::new(48_000, 3)).unwrap_err(),
            Error::Channels(3)
        );
        let mut c = Config::new(48_000, 1);
        c.bitrate = 1_000;
        assert!(Encoder::new(c).is_err());
        c.bitrate = 400_000;
        assert!(Encoder::new(c).is_err());
    }

    #[test]
    fn the_adts_header_carries_the_frame_length() {
        let h = adts_header(&Config::new(44_100, 2), 300);
        assert_eq!(&h[..3], &[0xFF, 0xF1, 0x50]);
        let len = (u32::from(h[3] & 3) << 11) | (u32::from(h[4]) << 3) | u32::from(h[5] >> 5);
        assert_eq!(len, 307);
        assert_eq!(h[3] >> 6, 2);
    }

    #[test]
    fn frames_cover_the_input_and_the_priming() {
        for n in [1usize, 1023, 1024, 1025, 5000] {
            let mut e = Encoder::new(Config::new(48_000, 1)).unwrap();
            let mut frames = e.encode(&vec![100; n]);
            frames.extend(e.finish());
            assert_eq!(frames.len(), n.div_ceil(1024) + 1, "{n}");
        }
        // Stereo samples split across calls are put back together.
        let mut e = Encoder::new(Config::new(48_000, 2)).unwrap();
        e.encode(&[1, 2, 3]);
        e.encode(&[4]);
        assert_eq!(e.samples(), 2);
    }

    #[test]
    fn a_frame_never_exceeds_what_a_decoder_buffers() {
        // White noise at full scale, as hard as input gets.
        let mut c = Config::new(48_000, 2);
        c.bitrate = 256_000;
        let mut e = Encoder::new(c).unwrap();
        let mut s = 1u32;
        let pcm: Vec<i16> = (0..48_000 * 2)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 16) as i16
            })
            .collect();
        let mut frames = e.encode(&pcm);
        frames.extend(e.finish());
        assert!(frames.iter().all(|f| f.len() * 8 <= 6144 * 2));
    }
}
