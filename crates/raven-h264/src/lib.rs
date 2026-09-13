//! Raven's own H.264 encoder.
//!
//! Constrained Baseline, the profile every H.264 decoder there is plays: CAVLC
//! entropy coding, I and P frames, one reference frame, one slice a picture.
//! Written for screen recordings, which is to say for content that mostly
//! does not move and, when it does, moves by whole pixels.
//!
//! The encoder keeps its own reconstruction — the picture a decoder will
//! produce — because that, not the source, is what the next P frame is
//! predicted from. It is also how the encoder is tested: a conforming decoder
//! fed this stream must reproduce [`Encoder::reconstruction_i420`] exactly.
//!
//! Input is 8-bit YUV 4:2:0, BT.709 limited range, with even dimensions.
//! Frames whose size is not a multiple of 16 are padded by repeating their
//! edges and cropped back in the SPS.

mod bits;
mod cavlc;
mod inter;
mod intra;
mod mb;
mod nal;
mod params;
mod transform;

use std::fmt;

pub use nal::annex_b;

use bits::BitWriter;

/// The highest quantiser: the smallest, worst-looking stream.
pub const MAX_QP: u8 = 51;

/// What to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Width in pixels; even.
    pub width: u32,
    /// Height in pixels; even.
    pub height: u32,
    /// The quantiser every macroblock is coded at, 0 to [`MAX_QP`]. Lower is
    /// better and bigger.
    pub qp: u8,
    /// The most frames a second the stream will carry, for choosing a level.
    pub frame_rate: u32,
}

impl Config {
    /// A `width` × `height` stream at the default quality.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            qp: 24,
            frame_rate: 30,
        }
    }
}

/// One frame of input: planar 8-bit YUV 4:2:0.
#[derive(Debug, Clone, Copy)]
pub struct Picture<'a> {
    /// `width × height` luma samples, top row first.
    pub y: &'a [u8],
    /// `width/2 × height/2` blue-difference samples.
    pub cb: &'a [u8],
    /// `width/2 × height/2` red-difference samples.
    pub cr: &'a [u8],
}

/// Everything that can stop an encode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Odd, zero, or larger than any H.264 level allows.
    BadDimensions {
        /// The width asked for.
        width: u32,
        /// The height asked for.
        height: u32,
    },
    /// A quantiser above [`MAX_QP`].
    BadQp(u8),
    /// Planes whose lengths do not match the configured size.
    WrongPictureSize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadDimensions { width, height } => {
                write!(f, "H.264 cannot encode a {width}x{height} picture")
            }
            Self::BadQp(qp) => write!(f, "quantiser {qp} is above {MAX_QP}"),
            Self::WrongPictureSize => f.write_str("a picture of the wrong size"),
        }
    }
}

impl std::error::Error for Error {}

/// One encoded frame.
#[derive(Debug)]
pub struct Encoded {
    /// Its NAL units, without start codes or length prefixes.
    pub nals: Vec<Vec<u8>>,
    /// Whether it is an IDR frame, which a player can start from.
    pub idr: bool,
}

/// A picture's three planes, padded to whole macroblocks.
#[derive(Debug, Clone)]
struct Planes {
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

/// The encoder.
pub struct Encoder {
    config: Config,
    mb_w: u32,
    mb_h: u32,
    sps: Vec<u8>,
    pps: Vec<u8>,
    frame_num: u32,
    idr_pic_id: u32,
    has_reference: bool,
    recon: Planes,
    mbs: mb::Macroblocks,
}

impl fmt::Debug for Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("config", &self.config)
            .field("frame_num", &self.frame_num)
            .finish_non_exhaustive()
    }
}

impl Encoder {
    /// An encoder for `config`.
    pub fn new(config: Config) -> Result<Self, Error> {
        let Config { width, height, .. } = config;
        let bad = Error::BadDimensions { width, height };
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(bad);
        }
        if config.qp > MAX_QP {
            return Err(Error::BadQp(config.qp));
        }
        let (mb_w, mb_h) = (width.div_ceil(16), height.div_ceil(16));
        let level = params::level(mb_w, mb_h, config.frame_rate).ok_or(bad)?;
        let (luma, chroma) = ((mb_w * mb_h * 256) as usize, (mb_w * mb_h * 64) as usize);
        Ok(Self {
            config,
            mb_w,
            mb_h,
            sps: params::sps(width, height, mb_w, mb_h, level),
            pps: params::pps(),
            frame_num: 0,
            idr_pic_id: 0,
            has_reference: false,
            mbs: mb::Macroblocks::new(mb_w as usize, mb_h as usize),
            recon: Planes {
                y: vec![0; luma],
                cb: vec![0; chroma],
                cr: vec![0; chroma],
            },
        })
    }

    /// The sequence parameter set, as a NAL unit.
    pub fn sps(&self) -> &[u8] {
        &self.sps
    }

    /// The picture parameter set, as a NAL unit.
    pub fn pps(&self) -> &[u8] {
        &self.pps
    }

    /// Encode the next frame. The first frame is always an IDR frame, and so
    /// is any with `key` set.
    pub fn encode(&mut self, picture: &Picture<'_>, key: bool) -> Result<Encoded, Error> {
        let source = self.pad(picture)?;
        let idr = key || !self.has_reference;
        let frame_num = if idr {
            0
        } else {
            (self.frame_num + 1) % (1 << params::FRAME_NUM_BITS)
        };

        let mut w = BitWriter::new();
        self.slice_header(&mut w, idr, frame_num);
        // Starts as the source, and each macroblock overwrites its own
        // samples with their reconstruction before the next reads them.
        let mut recon = source.clone();
        let qp = self.config.qp;
        self.mbs.begin_picture();
        // Skipped macroblocks are counted, and the count written in front of
        // the next one that is coded, or at the end of the slice.
        let mut skip_run = 0;
        for mby in 0..self.mb_h as usize {
            for mbx in 0..self.mb_w as usize {
                if idr {
                    self.mbs
                        .intra16(&mut w, &source, &mut recon, (mbx, mby), qp, false);
                } else {
                    self.mbs.p_macroblock(
                        &mut w,
                        &source,
                        &self.recon,
                        &mut recon,
                        (mbx, mby),
                        qp,
                        &mut skip_run,
                    );
                }
            }
        }
        if skip_run > 0 {
            w.ue(skip_run);
        }
        w.trailing();

        let nal = if idr {
            nal::wrap(3, nal::SLICE_IDR, &w.finish())
        } else {
            nal::wrap(2, nal::SLICE, &w.finish())
        };
        if idr {
            self.idr_pic_id = (self.idr_pic_id + 1) % 65_536;
        }
        self.frame_num = frame_num;
        self.has_reference = true;
        self.recon = recon;
        Ok(Encoded {
            nals: vec![nal],
            idr,
        })
    }

    /// What a decoder shows for the last frame encoded, cropped to the
    /// configured size, as consecutive Y, Cb and Cr planes.
    pub fn reconstruction_i420(&self) -> Vec<u8> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        let (stride, cstride) = ((self.mb_w * 16) as usize, (self.mb_w * 8) as usize);
        let mut out = Vec::with_capacity(w * h * 3 / 2);
        for row in 0..h {
            out.extend_from_slice(&self.recon.y[row * stride..row * stride + w]);
        }
        for plane in [&self.recon.cb, &self.recon.cr] {
            for row in 0..h / 2 {
                out.extend_from_slice(&plane[row * cstride..row * cstride + w / 2]);
            }
        }
        out
    }

    fn slice_header(&self, w: &mut BitWriter, idr: bool, frame_num: u32) {
        w.ue(0); // first_mb_in_slice
        w.ue(if idr { 7 } else { 5 }); // slice_type: I or P, for the whole picture
        w.ue(0); // pic_parameter_set_id
        w.put(params::FRAME_NUM_BITS, frame_num);
        if idr {
            w.ue(self.idr_pic_id);
        } else {
            w.flag(false); // num_ref_idx_active_override_flag
            w.flag(false); // ref_pic_list_modification_flag_l0
        }
        // dec_ref_pic_marking()
        if idr {
            w.flag(false); // no_output_of_prior_pics_flag
            w.flag(false); // long_term_reference_flag
        } else {
            w.flag(false); // adaptive_ref_pic_marking_mode_flag: sliding window
        }
        w.se(i32::from(self.config.qp) - 26); // slice_qp_delta
        w.ue(1); // disable_deblocking_filter_idc: off
    }

    /// The picture padded to whole macroblocks by repeating its last row and
    /// column.
    fn pad(&self, picture: &Picture<'_>) -> Result<Planes, Error> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        if picture.y.len() != w * h
            || picture.cb.len() != (w / 2) * (h / 2)
            || picture.cr.len() != (w / 2) * (h / 2)
        {
            return Err(Error::WrongPictureSize);
        }
        let (pw, ph) = ((self.mb_w * 16) as usize, (self.mb_h * 16) as usize);
        let pad = |plane: &[u8], w: usize, h: usize, pw: usize, ph: usize| {
            let mut out = Vec::with_capacity(pw * ph);
            for row in 0..ph {
                let src = &plane[row.min(h - 1) * w..row.min(h - 1) * w + w];
                out.extend_from_slice(src);
                out.resize(out.len() + pw - w, src[w - 1]);
            }
            out
        };
        Ok(Planes {
            y: pad(picture.y, w, h, pw, ph),
            cb: pad(picture.cb, w / 2, h / 2, pw / 2, ph / 2),
            cr: pad(picture.cr, w / 2, h / 2, pw / 2, ph / 2),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_configs_are_refused() {
        assert!(Encoder::new(Config::new(0, 10)).is_err());
        assert!(Encoder::new(Config::new(11, 10)).is_err());
        assert!(
            Encoder::new(Config {
                qp: 52,
                ..Config::new(16, 16)
            })
            .is_err()
        );
        assert!(Encoder::new(Config::new(1920, 1080)).is_ok());
    }

    #[test]
    fn the_wrong_picture_size_is_refused() {
        let mut encoder = Encoder::new(Config::new(16, 16)).unwrap();
        let (y, c) = (vec![0; 255], vec![0; 64]);
        let picture = Picture {
            y: &y,
            cb: &c,
            cr: &c,
        };
        assert_eq!(
            encoder.encode(&picture, false).unwrap_err(),
            Error::WrongPictureSize
        );
    }
}
