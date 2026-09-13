//! A minimal MP4 writer: one H.264 video track, variable frame timing.
//!
//! ISO/IEC 14496-12 and -15, which are fixed documents, so this does not rot.
//! Samples are written into `mdat` as they arrive, one chunk each, and the
//! index — `moov` — goes at the end, when every sample's size and time is
//! known. `mdat` carries a 64-bit size so a long recording cannot outgrow it.
//!
//! Frame timing is whatever the recording's was: a screen that sat still for
//! ten seconds is one frame lasting ten seconds, and `stts` says so.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Ticks per second in the track's timeline: the usual one for video.
pub(crate) const TIMESCALE: u32 = 90_000;

/// Ticks per second in the movie header, where only the total is kept.
const MOVIE_TIMESCALE: u32 = 1_000;

/// Where `mdat`'s 64-bit size lives: after `ftyp` (32 bytes: its header, the
/// major brand and version, four compatible brands) and the eight bytes of
/// `mdat`'s own compact header.
const MDAT_SIZE_AT: u64 = 32 + 8;

/// An MP4 being written.
#[derive(Debug)]
pub(crate) struct Mp4 {
    out: BufWriter<File>,
    width: u16,
    height: u16,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Byte offset, size, start tick and sync flag of each sample.
    samples: Vec<Sample>,
    position: u64,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    offset: u64,
    size: u32,
    start: u64,
    sync: bool,
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

/// The unity transform every header carries.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn put(body: &mut Vec<u8>, bytes: &[u8]) {
    body.extend_from_slice(bytes);
}

impl Mp4 {
    /// Start an MP4 at `path` for a `width` × `height` stream with these
    /// parameter sets, as NAL units.
    pub(crate) fn create(
        path: &Path,
        width: usize,
        height: usize,
        sps: &[u8],
        pps: &[u8],
    ) -> io::Result<Self> {
        let too_big = || io::Error::new(io::ErrorKind::InvalidInput, "a picture too large for MP4");
        let mut out = BufWriter::new(File::create(path)?);
        let mut ftyp = Vec::new();
        put(&mut ftyp, b"isom");
        put(&mut ftyp, &0x200u32.to_be_bytes());
        for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
            put(&mut ftyp, brand);
        }
        let ftyp = boxed(b"ftyp", &ftyp);
        debug_assert_eq!(ftyp.len() as u64 + 8, MDAT_SIZE_AT);
        out.write_all(&ftyp)?;
        // mdat, size 1 meaning "the 64-bit size follows", patched at the end.
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            out,
            width: u16::try_from(width).map_err(|_| too_big())?,
            height: u16::try_from(height).map_err(|_| too_big())?,
            sps: sps.to_vec(),
            pps: pps.to_vec(),
            samples: Vec::new(),
            position: MDAT_SIZE_AT + 8,
        })
    }

    /// Append a frame that starts at `start` ticks, as its NAL units.
    pub(crate) fn push(&mut self, start: u64, nals: &[Vec<u8>], sync: bool) -> io::Result<()> {
        let offset = self.position;
        let mut size = 0u64;
        for nal in nals {
            self.out.write_all(&(nal.len() as u32).to_be_bytes())?;
            self.out.write_all(nal)?;
            size += 4 + nal.len() as u64;
        }
        // Ticks only go forward: two frames in one tick would be a frame of
        // no length, which players drop.
        let start = match self.samples.last() {
            Some(last) => start.max(last.start + 1),
            None => start,
        };
        self.samples.push(Sample {
            offset,
            size: u32::try_from(size)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a frame over 4 GiB"))?,
            start,
            sync,
        });
        self.position += size;
        Ok(())
    }

    /// Finish: the last frame lasts until `end` ticks. Returns the file size.
    pub(crate) fn finish(mut self, end: u64) -> io::Result<u64> {
        let end = match self.samples.last() {
            Some(last) => end.max(last.start + 1),
            None => end,
        };
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
        let movie_duration = (end * u64::from(MOVIE_TIMESCALE) / u64::from(TIMESCALE)) as u32;

        let mut mvhd = Vec::new();
        put(&mut mvhd, &[0; 8]); // creation, modification
        put(&mut mvhd, &MOVIE_TIMESCALE.to_be_bytes());
        put(&mut mvhd, &movie_duration.to_be_bytes());
        put(&mut mvhd, &0x0001_0000u32.to_be_bytes()); // rate 1.0
        put(&mut mvhd, &0x0100u16.to_be_bytes()); // volume 1.0
        put(&mut mvhd, &[0; 10]);
        for m in MATRIX {
            put(&mut mvhd, &m.to_be_bytes());
        }
        put(&mut mvhd, &[0; 24]); // pre_defined
        put(&mut mvhd, &2u32.to_be_bytes()); // next_track_ID
        let mvhd = full(b"mvhd", 0, 0, &mvhd);

        let mut tkhd = Vec::new();
        put(&mut tkhd, &[0; 8]);
        put(&mut tkhd, &1u32.to_be_bytes()); // track_ID
        put(&mut tkhd, &[0; 4]);
        put(&mut tkhd, &movie_duration.to_be_bytes());
        put(&mut tkhd, &[0; 8]);
        put(&mut tkhd, &[0; 4]); // layer, alternate_group
        put(&mut tkhd, &[0; 4]); // volume, reserved
        for m in MATRIX {
            put(&mut tkhd, &m.to_be_bytes());
        }
        put(&mut tkhd, &(u32::from(self.width) << 16).to_be_bytes());
        put(&mut tkhd, &(u32::from(self.height) << 16).to_be_bytes());
        let tkhd = full(b"tkhd", 0, 3, &tkhd); // enabled, in movie

        let mut mdhd = Vec::new();
        put(&mut mdhd, &[0; 16]); // creation, modification (64-bit)
        put(&mut mdhd, &TIMESCALE.to_be_bytes());
        put(&mut mdhd, &end.to_be_bytes());
        put(&mut mdhd, &0x55C4u16.to_be_bytes()); // language "und"
        put(&mut mdhd, &[0; 2]);
        let mdhd = full(b"mdhd", 1, 0, &mdhd);

        let mut hdlr = Vec::new();
        put(&mut hdlr, &[0; 4]);
        put(&mut hdlr, b"vide");
        put(&mut hdlr, &[0; 12]);
        put(&mut hdlr, b"Raven screen recording\0");
        let hdlr = full(b"hdlr", 0, 0, &hdlr);

        let vmhd = full(b"vmhd", 0, 1, &[0; 8]);
        let dref = full(
            b"dref",
            0,
            0,
            &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat(),
        );
        let dinf = boxed(b"dinf", &dref);

        let stbl = boxed(
            b"stbl",
            &[
                self.stsd(),
                self.stts(end),
                self.stss(),
                full(
                    b"stsc",
                    0,
                    0,
                    &[1u32, 1, 1, 1].map(u32::to_be_bytes).concat(),
                ),
                self.stsz(),
                self.co64(),
            ]
            .concat(),
        );
        let minf = boxed(b"minf", &[vmhd, dinf, stbl].concat());
        let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
        let trak = boxed(b"trak", &[tkhd, mdia].concat());
        boxed(b"moov", &[mvhd, trak].concat())
    }

    fn stsd(&self) -> Vec<u8> {
        let mut avcc = vec![1, self.sps[1], self.sps[2], self.sps[3], 0xFF, 0xE1];
        put(&mut avcc, &(self.sps.len() as u16).to_be_bytes());
        put(&mut avcc, &self.sps);
        avcc.push(1);
        put(&mut avcc, &(self.pps.len() as u16).to_be_bytes());
        put(&mut avcc, &self.pps);

        // nclx: BT.709 primaries, transfer and matrix, limited range — the
        // same as the SPS says, for players that read only the container.
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
        put(&mut entry, &self.width.to_be_bytes());
        put(&mut entry, &self.height.to_be_bytes());
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

    /// Durations, run-length coded: a recording at a steady 30 Hz is one entry.
    fn stts(&self, end: u64) -> Vec<u8> {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (i, sample) in self.samples.iter().enumerate() {
            let next = self.samples.get(i + 1).map_or(end, |s| s.start);
            let delta = u32::try_from(next - sample.start).unwrap_or(u32::MAX);
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

    fn stss(&self) -> Vec<u8> {
        let sync: Vec<u32> = (1..)
            .zip(&self.samples)
            .filter(|(_, s)| s.sync)
            .map(|(n, _)| n)
            .collect();
        let mut body = (sync.len() as u32).to_be_bytes().to_vec();
        for n in sync {
            put(&mut body, &n.to_be_bytes());
        }
        full(b"stss", 0, 0, &body)
    }

    fn stsz(&self) -> Vec<u8> {
        let mut body = [0u32, self.samples.len() as u32]
            .map(u32::to_be_bytes)
            .concat();
        for sample in &self.samples {
            put(&mut body, &sample.size.to_be_bytes());
        }
        full(b"stsz", 0, 0, &body)
    }

    fn co64(&self) -> Vec<u8> {
        let mut body = (self.samples.len() as u32).to_be_bytes().to_vec();
        for sample in &self.samples {
            put(&mut body, &sample.offset.to_be_bytes());
        }
        full(b"co64", 0, 0, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Top-level boxes of an MP4, as (kind, size).
    fn top_level(bytes: &[u8]) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 8 <= bytes.len() {
            let mut size = u64::from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()));
            let kind = String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned();
            if size == 1 {
                size = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().unwrap());
            }
            out.push((kind, size));
            at += size as usize;
        }
        out
    }

    #[test]
    fn boxes_add_up_to_the_file() {
        let path =
            std::env::temp_dir().join(format!("raven-export-mp4-{}.mp4", std::process::id()));
        let sps = [0x67, 66, 0xC0, 41, 0xAA];
        let mut mp4 = Mp4::create(&path, 64, 48, &sps, &[0x68, 0xCE]).unwrap();
        mp4.push(0, &[vec![0x65, 1, 2, 3]], true).unwrap();
        mp4.push(3000, &[vec![0x41, 9]], false).unwrap();
        mp4.push(3000, &[vec![0x41, 8]], false).unwrap();
        let size = mp4.finish(90_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(bytes.len() as u64, size);
        let boxes = top_level(&bytes);
        let kinds: Vec<&str> = boxes.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["ftyp", "mdat", "moov"]);
        assert_eq!(boxes.iter().map(|(_, s)| s).sum::<u64>(), size);
        // Three samples of 8, 6 and 6 bytes behind the 16-byte mdat header.
        assert_eq!(boxes[1].1, 16 + 8 + 6 + 6);
    }
}
