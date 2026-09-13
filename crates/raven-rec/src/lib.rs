//! The Raven screen recording format.
//!
//! Huginn records the screen into this format; a separate exporter turns a
//! recording into a video other software plays. It is a format of our own for
//! the same reason the recorder is the compositor's own: nothing outside the
//! tree can change it or stop it building. It is lossless, it needs nothing
//! beyond `std`, and it is cheap enough to write in real time on a low-power
//! CPU, which no general-purpose video codec is.
//!
//! # Why it compresses
//!
//! A desktop is not camera footage. Most of a frame is the frame before it,
//! most of what did change is flat colour, and what is not flat very often
//! repeats the row above it — a title bar, the background behind a line of
//! text, a gradient running across. So each pixel is predicted three ways,
//! and the stream records which prediction held and for how many pixels:
//!
//! | Op | The pixel is |
//! |---|---|
//! | `UNCHANGED` | the same as this pixel in the previous frame |
//! | `LEFT` | the same as the pixel before it |
//! | `ABOVE` | the same as the pixel one row up |
//! | `LITERAL` | given raw, four bytes each |
//!
//! An unchanged screen is one op, a few bytes. A window dragged across a flat
//! wallpaper costs its outline. Only genuinely new detail — a photo scrolling
//! into view — costs its raw size, and that is the case an exporter's real
//! codec is for.
//!
//! # Layout
//!
//! Little-endian throughout.
//!
//! ```text
//! header  "RVNREC"  version u16  width u32  height u32  reserved u32    20 bytes
//! record  kind u8   pts u64      length u32 crc32 u32   payload         17 + length
//! ```
//!
//! `kind` is 0 for a key frame, 1 for a delta frame and 2 for the end marker.
//! `pts` is microseconds since the recording began and never goes backwards.
//! The CRC-32 (IEEE) covers the record's first 13 bytes and its payload.
//!
//! A frame's payload is a run of ops, in raster order, that covers the frame
//! exactly. Each op is a tag byte — the op in the top two bits, the run length
//! less one in the low six — and a low six of 63 means the length is 64 plus
//! the LEB128 varint that follows. A `LITERAL` op is followed by its pixels as
//! RGBA. Pixels are 8-bit RGBA, top row first.
//!
//! A key frame never uses `UNCHANGED`, so it decodes with no history. One is
//! written every [`KEY_INTERVAL`] frames, so a damaged file picks up again at
//! the next one and an exporter can seek.
//!
//! The end marker has an empty payload, and its `pts` is when recording
//! stopped, which is what says how long the last frame was on screen. A file
//! with no end marker was cut short — a crash, a power cut — and every complete
//! record before the break is still good.

use std::fmt;
use std::io::{self, Read, Write};
use std::time::Duration;

/// The first six bytes of every recording.
pub const MAGIC: [u8; 6] = *b"RVNREC";
/// The format version this crate writes and reads.
pub const VERSION: u16 = 1;
/// The file extension recordings are saved with.
pub const EXTENSION: &str = "rvr";
/// Size of the file header, in bytes.
pub const HEADER_LEN: usize = 20;
/// Size of a record's header, in bytes.
pub const RECORD_HEADER_LEN: usize = 17;
/// The largest width or height a recording may have. Past this a garbage
/// header could ask a reader to allocate gigabytes for one frame.
pub const MAX_DIMENSION: u32 = 16_384;
/// How many frames apart key frames are written: ten seconds at 30 Hz.
pub const KEY_INTERVAL: u32 = 300;

const KIND_KEY: u8 = 0;
const KIND_DELTA: u8 = 1;
const KIND_END: u8 = 2;

const OP_UNCHANGED: u8 = 0;
const OP_LEFT: u8 = 1;
const OP_ABOVE: u8 = 2;
const OP_LITERAL: u8 = 3;

/// Run lengths the tag byte holds on its own: 1 to 63. A low six of 63 means
/// a varint follows.
const SHORT_RUNS: usize = 63;

/// Whether a frame stands alone or builds on the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Decodes with no history.
    Key,
    /// Refers to the previous frame.
    Delta,
}

/// Everything that can go wrong writing or reading a recording.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying reader or writer failed.
    Io(io::Error),
    /// The file does not start with a recording header.
    NotARecording,
    /// A recording from a version of the format this crate does not read.
    UnsupportedVersion(u16),
    /// A width or height of zero, or larger than [`MAX_DIMENSION`].
    BadDimensions {
        /// The width asked for.
        width: u32,
        /// The height asked for.
        height: u32,
    },
    /// A frame handed to the encoder that is not `width × height × 4` bytes.
    WrongFrameSize {
        /// The size the recording's dimensions call for.
        expected: usize,
        /// The size given.
        got: usize,
    },
    /// A timestamp earlier than the one before it.
    TimeWentBackwards,
    /// The file ends partway through a record: the recording was cut short.
    /// Every frame before this one is good.
    Truncated,
    /// A record whose checksum does not match its contents.
    Checksum,
    /// A record of a kind this version does not know.
    UnknownRecord(u8),
    /// A delta frame with no picture to apply it to: the first frame of a
    /// file, or the first after a damaged one. Frames resume at the next key.
    DeltaWithoutKey,
    /// A record that passed its checksum but does not describe a frame.
    Corrupt(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::NotARecording => f.write_str("not a Raven recording"),
            Self::UnsupportedVersion(v) => {
                write!(f, "recording format version {v} is not supported")
            }
            Self::BadDimensions { width, height } => {
                write!(f, "a {width}x{height} recording is not possible")
            }
            Self::WrongFrameSize { expected, got } => {
                write!(f, "a frame of {got} bytes where {expected} were expected")
            }
            Self::TimeWentBackwards => f.write_str("a frame timestamped before the one before it"),
            Self::Truncated => f.write_str("the recording was cut short"),
            Self::Checksum => f.write_str("a damaged record"),
            Self::UnknownRecord(kind) => write!(f, "an unknown record kind {kind}"),
            Self::DeltaWithoutKey => f.write_str("a delta frame with no key frame before it"),
            Self::Corrupt(what) => write!(f, "a corrupt frame: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// This crate's result.
pub type Result<T> = std::result::Result<T, Error>;

/// Writes a recording.
pub struct Encoder<W: Write> {
    out: W,
    width: usize,
    pixels: usize,
    /// The last frame pushed, which the next delta is taken against.
    previous: Vec<u8>,
    has_previous: bool,
    /// Frames written since the last key frame.
    since_key: u32,
    key_interval: u32,
    last_pts: u64,
    /// Reused between frames, so a recording allocates its scratch once.
    payload: Vec<u8>,
}

impl<W: Write> fmt::Debug for Encoder<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("width", &self.width)
            .field("pixels", &self.pixels)
            .field("since_key", &self.since_key)
            .field("last_pts", &self.last_pts)
            .finish_non_exhaustive()
    }
}

impl<W: Write> Encoder<W> {
    /// Start a `width` × `height` recording, writing its header to `out`.
    pub fn new(mut out: W, width: u32, height: u32) -> Result<Self> {
        let pixels = check_dimensions(width, height)?;
        let mut header = [0u8; HEADER_LEN];
        header[0..6].copy_from_slice(&MAGIC);
        header[6..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..12].copy_from_slice(&width.to_le_bytes());
        header[12..16].copy_from_slice(&height.to_le_bytes());
        out.write_all(&header)?;
        Ok(Self {
            out,
            width: width as usize,
            pixels,
            previous: Vec::new(),
            has_previous: false,
            since_key: 0,
            key_interval: KEY_INTERVAL,
            last_pts: 0,
            payload: Vec::new(),
        })
    }

    /// Write a key frame every `frames` frames instead of every
    /// [`KEY_INTERVAL`]. One means every frame is a key frame.
    pub fn with_key_interval(mut self, frames: u32) -> Self {
        self.key_interval = frames.max(1);
        self
    }

    /// The writer, for looking at what has been written so far.
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// Append a frame shown at `pts`, as 8-bit RGBA, top row first.
    pub fn push(&mut self, pts: Duration, rgba: &[u8]) -> Result<FrameKind> {
        let expected = self.pixels * 4;
        if rgba.len() != expected {
            return Err(Error::WrongFrameSize {
                expected,
                got: rgba.len(),
            });
        }
        let pts = self.check_pts(pts)?;
        let kind = if !self.has_previous || self.since_key + 1 >= self.key_interval {
            FrameKind::Key
        } else {
            FrameKind::Delta
        };

        self.payload.clear();
        let previous = (kind == FrameKind::Delta).then_some(self.previous.as_slice());
        encode(rgba, previous, self.width, &mut self.payload);
        let byte = match kind {
            FrameKind::Key => KIND_KEY,
            FrameKind::Delta => KIND_DELTA,
        };
        self.write_record(byte, pts)?;

        self.previous.clear();
        self.previous.extend_from_slice(rgba);
        self.has_previous = true;
        self.last_pts = pts;
        self.since_key = match kind {
            FrameKind::Key => 0,
            FrameKind::Delta => self.since_key + 1,
        };
        Ok(kind)
    }

    /// Flush what has been written without ending the recording.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    /// End the recording at `pts`, flush, and hand the writer back.
    pub fn finish(mut self, pts: Duration) -> Result<W> {
        let pts = self.check_pts(pts)?;
        self.payload.clear();
        self.write_record(KIND_END, pts)?;
        self.out.flush()?;
        Ok(self.out)
    }

    fn check_pts(&self, pts: Duration) -> Result<u64> {
        let pts = u64::try_from(pts.as_micros()).unwrap_or(u64::MAX);
        if pts < self.last_pts {
            return Err(Error::TimeWentBackwards);
        }
        Ok(pts)
    }

    fn write_record(&mut self, kind: u8, pts: u64) -> Result<()> {
        // Unreachable within MAX_DIMENSION — the worst frame is five bytes a
        // pixel — but a length that silently wrapped would corrupt everything
        // after it, so it is checked rather than assumed.
        let length = u32::try_from(self.payload.len())
            .map_err(|_| Error::Corrupt("a frame too large to record"))?;
        let mut head = [0u8; RECORD_HEADER_LEN];
        head[0] = kind;
        head[1..9].copy_from_slice(&pts.to_le_bytes());
        head[9..13].copy_from_slice(&length.to_le_bytes());
        let crc = crc32(&[&head[..13], &self.payload]);
        head[13..17].copy_from_slice(&crc.to_le_bytes());
        self.out.write_all(&head)?;
        self.out.write_all(&self.payload)?;
        Ok(())
    }
}

/// One decoded frame.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    /// When it was shown, from the start of the recording.
    pub pts: Duration,
    /// Whether it was stored as a key frame or a delta.
    pub kind: FrameKind,
    /// The whole picture, 8-bit RGBA, top row first.
    pub rgba: &'a [u8],
}

impl fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("pts", &self.pts)
            .field("kind", &self.kind)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

/// Reads a recording.
///
/// A damaged record — [`Error::Checksum`], [`Error::UnknownRecord`],
/// [`Error::Corrupt`] — leaves the decoder at the record after it, so reading
/// can carry on: deltas report [`Error::DeltaWithoutKey`] until the next key
/// frame puts the picture back. A header damaged badly enough to misstate its
/// own length cannot be stepped over, and what follows will fail its
/// checksums. [`Error::Truncated`] and [`Error::Io`] end the recording.
pub struct Decoder<R: Read> {
    input: R,
    width: u32,
    height: u32,
    /// The picture as of the last frame decoded, and the one the next delta
    /// is applied to, in place.
    frame: Vec<u8>,
    has_frame: bool,
    last_pts: u64,
    payload: Vec<u8>,
    end: Option<Duration>,
}

impl<R: Read> fmt::Debug for Decoder<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("has_frame", &self.has_frame)
            .field("last_pts", &self.last_pts)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl<R: Read> Decoder<R> {
    /// Open a recording, reading its header.
    pub fn new(mut input: R) -> Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        if !matches!(read_full(&mut input, &mut header)?, Filled::Complete) {
            return Err(Error::NotARecording);
        }
        if header[0..6] != MAGIC {
            return Err(Error::NotARecording);
        }
        let version = u16::from_le_bytes([header[6], header[7]]);
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let width = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let height = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        let pixels = check_dimensions(width, height)?;
        Ok(Self {
            input,
            width,
            height,
            frame: vec![0; pixels * 4],
            has_frame: false,
            last_pts: 0,
            payload: Vec::new(),
            end: None,
        })
    }

    /// The recording's width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The recording's height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// When recording stopped, once the end marker has been read. `None`
    /// before that, and for a recording that was cut short.
    pub fn end(&self) -> Option<Duration> {
        self.end
    }

    /// The next frame, or `None` at the end of the recording.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>> {
        if self.end.is_some() {
            return Ok(None);
        }
        let mut head = [0u8; RECORD_HEADER_LEN];
        match read_full(&mut self.input, &mut head)? {
            Filled::Complete => {}
            // Ending exactly between records is a recording that stopped
            // without its end marker, not a damaged one.
            Filled::Empty => return Ok(None),
            Filled::Partial => return Err(Error::Truncated),
        }
        let kind = head[0];
        let pts = u64::from_le_bytes(head[1..9].try_into().expect("eight bytes"));
        let length = u32::from_le_bytes(head[9..13].try_into().expect("four bytes")) as usize;
        let stored = u32::from_le_bytes(head[13..17].try_into().expect("four bytes"));

        // Checked before allocating, so a garbage length cannot ask for
        // gigabytes. Five bytes a pixel is more than the worst real frame.
        let pixels = self.frame.len() / 4;
        if length > pixels * 5 + 16 {
            self.has_frame = false;
            return Err(Error::Corrupt("a record longer than any frame could be"));
        }
        self.payload.resize(length, 0);
        if !matches!(
            read_full(&mut self.input, &mut self.payload)?,
            Filled::Complete
        ) {
            return Err(Error::Truncated);
        }
        if crc32(&[&head[..13], &self.payload]) != stored {
            self.has_frame = false;
            return Err(Error::Checksum);
        }
        if pts < self.last_pts {
            self.has_frame = false;
            return Err(Error::TimeWentBackwards);
        }
        self.last_pts = pts;
        let pts = Duration::from_micros(pts);

        let kind = match kind {
            KIND_KEY => FrameKind::Key,
            KIND_DELTA => FrameKind::Delta,
            KIND_END => {
                if length != 0 {
                    return Err(Error::Corrupt("an end marker with a payload"));
                }
                self.end = Some(pts);
                return Ok(None);
            }
            other => {
                self.has_frame = false;
                return Err(Error::UnknownRecord(other));
            }
        };
        if kind == FrameKind::Delta && !self.has_frame {
            return Err(Error::DeltaWithoutKey);
        }
        let key = kind == FrameKind::Key;
        if let Err(e) = decode(&self.payload, &mut self.frame, self.width as usize, key) {
            self.has_frame = false;
            return Err(e);
        }
        self.has_frame = true;
        Ok(Some(Frame {
            pts,
            kind,
            rgba: &self.frame,
        }))
    }
}

/// The pixel count of a `width` × `height` frame, if it is one a recording
/// may have.
fn check_dimensions(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::BadDimensions { width, height });
    }
    Ok(width as usize * height as usize)
}

/// Encode `current` as ops, against `previous` when there is one.
///
/// Greedy: at each pixel the longest of the three predictions wins, and a
/// pixel none of them predicts joins the literal run. Any run at all beats a
/// literal — a run of one is a tag byte, a literal pixel four — so there is no
/// threshold to tune. Each prediction stops counting at its first miss and
/// none can outrun the winner, so the work is linear in the frame.
fn encode(current: &[u8], previous: Option<&[u8]>, width: usize, out: &mut Vec<u8>) {
    let pixels = current.len() / 4;
    let mut i = 0;
    let mut literal_from = None;
    while i < pixels {
        let rest = &current[i * 4..];
        // Ties go to the earlier candidate, cheapest to decode first:
        // unchanged is no work at all.
        let mut best = (OP_LITERAL, 0);
        if let Some(previous) = previous {
            best = (OP_UNCHANGED, common_run(rest, &previous[i * 4..]));
        }
        if i >= width {
            let run = common_run(rest, &current[(i - width) * 4..]);
            if run > best.1 {
                best = (OP_ABOVE, run);
            }
        }
        if i >= 1 {
            let run = common_run(rest, &current[(i - 1) * 4..]);
            if run > best.1 {
                best = (OP_LEFT, run);
            }
        }
        if best.1 == 0 {
            literal_from.get_or_insert(i);
            i += 1;
            continue;
        }
        if let Some(from) = literal_from.take() {
            emit(out, OP_LITERAL, i - from);
            out.extend_from_slice(&current[from * 4..i * 4]);
        }
        emit(out, best.0, best.1);
        i += best.1;
    }
    if let Some(from) = literal_from {
        emit(out, OP_LITERAL, pixels - from);
        out.extend_from_slice(&current[from * 4..]);
    }
}

/// How many whole pixels `a` and `b` agree on from their starts.
///
/// The slices may overlap — `LEFT` compares a frame against itself one pixel
/// back — which is fine, since both are only read. Compared a block at a time
/// first, because the long runs are the common case and a slice comparison of
/// 64 bytes is one `memcmp`, not sixteen pixel checks.
fn common_run(a: &[u8], b: &[u8]) -> usize {
    const BLOCK: usize = 64;
    let len = a.len().min(b.len()) / 4 * 4;
    let mut at = 0;
    while at + BLOCK <= len && a[at..at + BLOCK] == b[at..at + BLOCK] {
        at += BLOCK;
    }
    while at + 4 <= len && a[at..at + 4] == b[at..at + 4] {
        at += 4;
    }
    at / 4
}

/// Write one op's tag, and its varint when the run is too long for the tag.
fn emit(out: &mut Vec<u8>, op: u8, run: usize) {
    debug_assert!(run >= 1, "an op covers at least one pixel");
    let short = run - 1;
    if short < SHORT_RUNS {
        out.push((op << 6) | short as u8);
    } else {
        out.push((op << 6) | SHORT_RUNS as u8);
        let mut rest = (short - SHORT_RUNS) as u64;
        loop {
            let byte = (rest & 0x7F) as u8;
            rest >>= 7;
            if rest == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
}

/// Apply a frame's ops to `frame`, in place.
///
/// In place works because every op reads only what is already final: the
/// previous frame at the same position (`UNCHANGED`, which is therefore no
/// work) or pixels of this frame already decoded (`LEFT`, `ABOVE`). Corrupt
/// input is an error, never a panic or a read out of bounds.
fn decode(payload: &[u8], frame: &mut [u8], width: usize, key: bool) -> Result<()> {
    let pixels = frame.len() / 4;
    let mut at = 0;
    let mut i = 0;
    while i < pixels {
        let tag = *payload
            .get(at)
            .ok_or(Error::Corrupt("a frame that stops before its last pixel"))?;
        at += 1;
        let op = tag >> 6;
        let short = usize::from(tag & 0x3F);
        let run = if short < SHORT_RUNS {
            short + 1
        } else {
            let (extra, used) = read_varint(&payload[at..])?;
            at += used;
            usize::try_from(extra)
                .ok()
                .and_then(|extra| extra.checked_add(SHORT_RUNS + 1))
                .ok_or(Error::Corrupt("a run longer than the frame"))?
        };
        let end = i
            .checked_add(run)
            .filter(|&end| end <= pixels)
            .ok_or(Error::Corrupt("a run longer than the frame"))?;
        match op {
            OP_UNCHANGED => {
                if key {
                    return Err(Error::Corrupt(
                        "a key frame that refers to the frame before it",
                    ));
                }
            }
            OP_LEFT => {
                if i == 0 {
                    return Err(Error::Corrupt("a first pixel copied from the left"));
                }
                let pixel: [u8; 4] = frame[(i - 1) * 4..i * 4].try_into().expect("four bytes");
                let (run_pixels, _) = frame[i * 4..end * 4].as_chunks_mut::<4>();
                run_pixels.fill(pixel);
            }
            OP_ABOVE => {
                if i < width {
                    return Err(Error::Corrupt("a first-row pixel copied from above"));
                }
                // At most a row at a time, so no copy reads a pixel the same
                // copy writes: a run longer than the width repeats rows it
                // has itself just produced.
                let mut p = i;
                while p < end {
                    let n = (end - p).min(width);
                    frame.copy_within((p - width) * 4..(p - width + n) * 4, p * 4);
                    p += n;
                }
            }
            _ => {
                let bytes = payload
                    .get(at..at + run * 4)
                    .ok_or(Error::Corrupt("a literal run cut short"))?;
                frame[i * 4..end * 4].copy_from_slice(bytes);
                at += run * 4;
            }
        }
        i = end;
    }
    if at != payload.len() {
        return Err(Error::Corrupt("bytes after the last pixel"));
    }
    Ok(())
}

/// A LEB128 varint from the start of `bytes`, and how many bytes it took.
fn read_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in bytes.iter().enumerate().take(10) {
        let bits = u64::from(byte & 0x7F);
        let shift = 7 * index as u32;
        if shift == 63 && bits > 1 {
            return Err(Error::Corrupt("a run length past 64 bits"));
        }
        value |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(Error::Corrupt("an unterminated run length"))
}

enum Filled {
    Complete,
    /// Nothing at all before the end of the input.
    Empty,
    /// Some, but not all, before the end of the input.
    Partial,
}

/// Fill `buf` from `input`, saying how far it got if the input ended first.
fn read_full(input: &mut impl Read, buf: &mut [u8]) -> io::Result<Filled> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(Filled::Empty),
            Ok(0) => return Ok(Filled::Partial),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Filled::Complete)
}

/// CRC-32 lookup tables, slicing-by-8.
///
/// Eight tables rather than the textbook one because a frame of fresh detail
/// is megabytes, the writer checksums it on a low-power CPU thirty times a
/// second, and a byte at a time is the difference between keeping up and not.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        tables[0][n] = c;
        n += 1;
    }
    let mut n = 0;
    while n < 256 {
        let mut t = 1;
        while t < 8 {
            let previous = tables[t - 1][n];
            tables[t][n] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            t += 1;
        }
        n += 1;
    }
    tables
};

/// CRC-32 (IEEE) of `parts`, one after another.
fn crc32(parts: &[&[u8]]) -> u32 {
    let t = &CRC_TABLES;
    let mut crc = !0u32;
    for part in parts {
        let (chunks, remainder) = part.as_chunks::<8>();
        for c in chunks {
            let low = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            crc = t[7][(low & 0xFF) as usize]
                ^ t[6][((low >> 8) & 0xFF) as usize]
                ^ t[5][((low >> 16) & 0xFF) as usize]
                ^ t[4][(low >> 24) as usize]
                ^ t[3][usize::from(c[4])]
                ^ t[2][usize::from(c[5])]
                ^ t[1][usize::from(c[6])]
                ^ t[0][usize::from(c[7])];
        }
        for &byte in remainder {
            crc = t[0][((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Deterministic noise: the frame no prediction helps with.
    fn noise(width: usize, height: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..width * height * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn solid(width: usize, height: usize, pixel: [u8; 4]) -> Vec<u8> {
        pixel.repeat(width * height)
    }

    fn fill_rect(
        frame: &mut [u8],
        width: usize,
        (x, y, w, h): (usize, usize, usize, usize),
        pixel: [u8; 4],
    ) {
        for row in y..y + h {
            for col in x..x + w {
                let at = (row * width + col) * 4;
                frame[at..at + 4].copy_from_slice(&pixel);
            }
        }
    }

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b""]), 0);
    }

    #[test]
    fn sliced_crc_agrees_with_a_byte_at_a_time() {
        let data = noise(37, 11, 7);
        for len in [0, 1, 7, 8, 9, 63, 64, 65, data.len()] {
            let mut slow = !0u32;
            for &byte in &data[..len] {
                slow = CRC_TABLES[0][((slow ^ u32::from(byte)) & 0xFF) as usize] ^ (slow >> 8);
            }
            assert_eq!(crc32(&[&data[..len]]), !slow, "length {len}");
        }
    }

    #[test]
    fn frames_round_trip_through_keys_and_deltas() {
        let (w, h) = (97, 61);
        let mut frames = vec![noise(w, h, 1)];
        // A window opens: a flat rectangle over the noise.
        let mut next = frames[0].clone();
        fill_rect(&mut next, w, (10, 5, 40, 30), [30, 30, 40, 255]);
        frames.push(next);
        // Everything scrolls up a row.
        let mut next = frames[1].clone();
        next.copy_within(w * 4.., 0);
        frames.push(next);
        // Nothing moves.
        frames.push(frames[2].clone());
        // Something entirely new.
        frames.push(noise(w, h, 99));

        let mut encoder = Encoder::new(Vec::new(), w as u32, h as u32)
            .unwrap()
            .with_key_interval(3);
        let mut kinds = Vec::new();
        for (n, frame) in frames.iter().enumerate() {
            kinds.push(encoder.push(ms(n as u64 * 33), frame).unwrap());
        }
        let bytes = encoder.finish(ms(200)).unwrap();
        use FrameKind::{Delta, Key};
        assert_eq!(kinds, [Key, Delta, Delta, Key, Delta]);

        let mut decoder = Decoder::new(bytes.as_slice()).unwrap();
        assert_eq!((decoder.width(), decoder.height()), (97, 61));
        for (n, expected) in frames.iter().enumerate() {
            let frame = decoder.next_frame().unwrap().expect("a frame");
            assert_eq!(frame.pts, ms(n as u64 * 33));
            assert_eq!(frame.kind, kinds[n]);
            assert!(frame.rgba == expected.as_slice(), "frame {n} differs");
        }
        assert!(decoder.next_frame().unwrap().is_none());
        assert_eq!(decoder.end(), Some(ms(200)));
    }

    #[test]
    fn an_unchanged_frame_costs_a_few_bytes() {
        let (w, h) = (320, 200);
        let frame = noise(w, h, 3);
        let mut encoder = Encoder::new(Vec::new(), w as u32, h as u32).unwrap();
        encoder.push(ms(0), &frame).unwrap();
        let before = encoder.get_ref().len();
        encoder.push(ms(33), &frame).unwrap();
        let cost = encoder.get_ref().len() - before;
        assert!(
            cost <= RECORD_HEADER_LEN + 4,
            "an idle frame took {cost} bytes"
        );
    }

    #[test]
    fn a_flat_screen_is_a_handful_of_bytes_even_as_a_key_frame() {
        let (w, h) = (1920, 1080);
        let mut payload = Vec::new();
        encode(&solid(w, h, [20, 20, 30, 255]), None, w, &mut payload);
        assert!(
            payload.len() <= 16,
            "a flat key frame took {} bytes",
            payload.len()
        );
    }

    #[test]
    fn a_key_frame_decodes_over_any_picture() {
        let (w, h) = (50, 20);
        let mut frame = noise(w, h, 5);
        fill_rect(&mut frame, w, (0, 0, 50, 8), [1, 2, 3, 255]);
        let mut payload = Vec::new();
        encode(&frame, None, w, &mut payload);
        let mut out = vec![0xAB; frame.len()];
        decode(&payload, &mut out, w, true).unwrap();
        assert!(out == frame);
    }

    #[test]
    fn runs_either_side_of_the_varint_boundary_round_trip() {
        for w in [1, 2, 63, 64, 65, 66, 129, 70_001] {
            let mut frame = solid(w, 2, [9, 8, 7, 255]);
            // A lone odd pixel on the second row, so ABOVE runs break too.
            if w > 1 {
                frame[(w + w / 2) * 4] = 0;
            }
            let mut payload = Vec::new();
            encode(&frame, None, w, &mut payload);
            let mut out = vec![0; frame.len()];
            decode(&payload, &mut out, w, true).unwrap();
            assert!(out == frame, "width {w}");
        }
    }

    #[test]
    fn a_recording_cut_short_keeps_every_whole_frame() {
        let (w, h) = (40, 30);
        let frames = [noise(w, h, 1), noise(w, h, 2), noise(w, h, 3)];
        let mut encoder = Encoder::new(Vec::new(), w as u32, h as u32).unwrap();
        for (n, frame) in frames.iter().enumerate() {
            encoder.push(ms(n as u64), frame).unwrap();
        }
        // No end marker, and the last record loses its tail.
        let mut bytes = encoder.get_ref().clone();
        bytes.truncate(bytes.len() - 5);

        let mut decoder = Decoder::new(bytes.as_slice()).unwrap();
        for expected in &frames[..2] {
            assert!(decoder.next_frame().unwrap().unwrap().rgba == expected.as_slice());
        }
        assert!(matches!(decoder.next_frame(), Err(Error::Truncated)));
    }

    #[test]
    fn a_recording_without_an_end_marker_just_ends() {
        let mut encoder = Encoder::new(Vec::new(), 4, 4).unwrap();
        encoder.push(ms(0), &solid(4, 4, [1, 1, 1, 255])).unwrap();
        let bytes = encoder.get_ref().clone();
        let mut decoder = Decoder::new(bytes.as_slice()).unwrap();
        assert!(decoder.next_frame().unwrap().is_some());
        assert!(decoder.next_frame().unwrap().is_none());
        assert_eq!(decoder.end(), None);
    }

    #[test]
    fn a_damaged_record_is_skipped_and_the_next_key_frame_recovers() {
        let (w, h) = (30, 20);
        let frames: Vec<_> = (1..=5).map(|seed| noise(w, h, seed)).collect();
        let mut encoder = Encoder::new(Vec::new(), w as u32, h as u32)
            .unwrap()
            .with_key_interval(3);
        let mut offsets = Vec::new();
        for (n, frame) in frames.iter().enumerate() {
            offsets.push(encoder.get_ref().len());
            encoder.push(ms(n as u64), frame).unwrap();
        }
        let mut bytes = encoder.finish(ms(10)).unwrap();
        // Frames are K D D K D; damage the payload of the first delta.
        bytes[offsets[1] + RECORD_HEADER_LEN] ^= 0xFF;

        let mut decoder = Decoder::new(bytes.as_slice()).unwrap();
        assert!(decoder.next_frame().unwrap().unwrap().rgba == frames[0].as_slice());
        assert!(matches!(decoder.next_frame(), Err(Error::Checksum)));
        assert!(matches!(decoder.next_frame(), Err(Error::DeltaWithoutKey)));
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame.kind, FrameKind::Key);
        assert!(frame.rgba == frames[3].as_slice());
        assert!(decoder.next_frame().unwrap().unwrap().rgba == frames[4].as_slice());
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn the_encoder_refuses_bad_input() {
        assert!(matches!(
            Encoder::new(Vec::new(), 0, 10),
            Err(Error::BadDimensions { .. })
        ));
        assert!(matches!(
            Encoder::new(Vec::new(), MAX_DIMENSION + 1, 10),
            Err(Error::BadDimensions { .. })
        ));
        let mut encoder = Encoder::new(Vec::new(), 2, 2).unwrap();
        assert!(matches!(
            encoder.push(ms(0), &[0; 15]),
            Err(Error::WrongFrameSize {
                expected: 16,
                got: 15
            })
        ));
        encoder.push(ms(10), &[0; 16]).unwrap();
        assert!(matches!(
            encoder.push(ms(5), &[0; 16]),
            Err(Error::TimeWentBackwards)
        ));
    }

    #[test]
    fn the_decoder_refuses_what_is_not_a_recording() {
        assert!(matches!(
            Decoder::new(&b"definitely not a recording"[..]),
            Err(Error::NotARecording)
        ));
        assert!(matches!(
            Decoder::new(&b"RVN"[..]),
            Err(Error::NotARecording)
        ));
        let mut header = Encoder::new(Vec::new(), 2, 2).unwrap().get_ref().clone();
        header[6] = 2;
        assert!(matches!(
            Decoder::new(header.as_slice()),
            Err(Error::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn malformed_ops_are_errors_not_panics() {
        let tag = |op: u8, short: u8| (op << 6) | short;
        let cases: [(&[u8], usize, usize, bool); 7] = [
            (&[tag(OP_LEFT, 0)], 1, 1, true),
            (&[tag(OP_ABOVE, 0), tag(OP_ABOVE, 0)], 2, 2, false),
            (&[tag(OP_UNCHANGED, 0)], 1, 1, true),
            (&[tag(OP_UNCHANGED, 1)], 1, 1, false),
            (&[tag(OP_UNCHANGED, 0), 0], 1, 1, false),
            (&[tag(OP_LITERAL, 0), 1, 2], 1, 1, false),
            (&[tag(OP_UNCHANGED, 0)], 2, 1, false),
        ];
        for (n, (payload, w, h, key)) in cases.into_iter().enumerate() {
            let mut frame = vec![0; w * h * 4];
            assert!(
                matches!(decode(payload, &mut frame, w, key), Err(Error::Corrupt(_))),
                "case {n} was accepted"
            );
        }
        // An unterminated varint and one past 64 bits.
        let mut frame = vec![0; 4];
        assert!(decode(&[tag(OP_UNCHANGED, 63), 0x80], &mut frame, 1, false).is_err());
        let huge = [
            tag(OP_UNCHANGED, 63),
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0x7F,
        ];
        assert!(decode(&huge, &mut frame, 1, false).is_err());
    }
}
