//! The picture the desktop is drawn on.
//!
//! # Where it comes from
//!
//! `/usr/share/wallpaper` is the machine's library of images and
//! `/usr/share/wallpaper/set` holds the one that is on, named `wallpaper` with
//! whatever extension it arrived with. Both paths are compiled in rather than
//! configured, because RavenLogin's greeter reads the same two: it is the
//! contract that makes the login screen and the session behind it show the
//! same picture, and a contract a config file can move is not one.
//!
//! The extension is a label. PNG and JPEG are told apart by the first bytes,
//! so an image renamed on the way in still draws -- which matters more here
//! than it looks, since the file is usually a symlink somebody made by hand.
//!
//! # What is trusted
//!
//! This decodes a file the machine's administrator put in a root-owned
//! directory, which is a weaker claim than it sounds: the compositor is the
//! one process on the desktop that must not die. So the decoders are
//! pure-Rust, they are given explicit limits before they allocate anything,
//! and every failure returns `None` and leaves the plain background in place.
//! A desktop with no wallpaper is a desktop; a compositor that exited is not.
//!
//! # Why it is scaled here and not by the renderer
//!
//! Scaling once into a buffer costs one pass over the output's pixels and
//! happens when an output is configured -- so, in practice, at startup and on
//! a mode change. Handing the renderer the source image and a destination
//! rectangle instead would resample it on every frame, forever, to draw a
//! picture that never changes.

use std::path::PathBuf;

use huginn_core::geometry::Size;

use crate::canvas::{Canvas, Panel};

/// Where the machine keeps the wallpaper it is using.
const SET_DIR: &str = "/usr/share/wallpaper/set";

/// The basename of the active wallpaper inside [`SET_DIR`].
const SET_STEM: &str = "wallpaper";

/// The largest file this will read, before any decoding.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The largest image this will decode, in pixels. About an 8000x8000 image,
/// and roughly 256 MiB once it is RGBA, which is the real ceiling being set.
const MAX_PIXELS: usize = 64 * 1_000_000;

/// The largest single dimension. Separate from [`MAX_PIXELS`] because that is
/// an area: a 100000x2 image is only 200k pixels and is still nothing anybody
/// meant to set as a wallpaper.
const MAX_DIMENSION: usize = 32_768;

/// A decoded wallpaper, at its own size.
pub(crate) struct Wallpaper {
    width: u32,
    height: u32,
    /// R, G, B, A per pixel -- [`Canvas`]'s order, so composing is a copy.
    pixels: Vec<u8>,
}

impl std::fmt::Debug for Wallpaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written: the derived one would format several megabytes of
        // pixels into a log line.
        f.debug_struct("Wallpaper")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl Wallpaper {
    /// The wallpaper this machine has set, if it has one that decodes.
    ///
    /// A missing directory is silent -- it is the ordinary state of a machine
    /// nobody has set one on. A file that is there and will not decode is a
    /// warning, because somebody did something and it did not work.
    pub(crate) fn installed() -> Option<Self> {
        let path = installed_path()?;
        match load(&path) {
            Ok(wallpaper) => {
                tracing::info!(
                    path = %path.display(),
                    width = wallpaper.width,
                    height = wallpaper.height,
                    "wallpaper loaded"
                );
                Some(wallpaper)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "ignoring the wallpaper: {e:#}");
                None
            }
        }
    }

    /// Compose it for an output: `render` pixels, marked as `density`.
    ///
    /// `render` and not `physical`, so the wallpaper is composed at the same
    /// size as every other surface in the scene and the fractional downsample
    /// to the panel happens once, to all of it, in the renderer. See
    /// `huginn_core::scale`.
    pub(crate) fn panel(&self, render: Size, density: u32) -> Panel {
        let (w, h) = (render.w.max(1) as usize, render.h.max(1) as usize);
        let mut canvas = Canvas::new(w, h);
        canvas.pixels = self.scaled_to_cover(w, h);
        Panel::from_canvas(&canvas, density)
    }

    /// Scale to fill `dst_w` x `dst_h` exactly, cropping the overflowing axis.
    ///
    /// "Cover" rather than "fit" because a letterboxed wallpaper looks like a
    /// mistake. The greeter does the same to the same image, so a login screen
    /// that hands over to this desktop does not appear to move the picture.
    ///
    /// Unlike the greeter's, nothing is darkened: the login screen dims its
    /// wallpaper so a password field stays readable on top of it, and a
    /// desktop has no such text of its own.
    fn scaled_to_cover(&self, dst_w: usize, dst_h: usize) -> Vec<u8> {
        let (src_w, src_h) = (self.width.max(1) as usize, self.height.max(1) as usize);

        // The larger of the two ratios is the one that leaves no gap.
        let scale = (dst_w as f32 / src_w as f32).max(dst_h as f32 / src_h as f32);
        // How much of the source is visible, and where it starts, so the crop
        // takes the middle of the image rather than its top-left corner.
        let window_w = (dst_w as f32 / scale).min(src_w as f32);
        let window_h = (dst_h as f32 / scale).min(src_h as f32);
        let origin_x = (src_w as f32 - window_w) / 2.0;
        let origin_y = (src_h as f32 - window_h) / 2.0;

        let mut out = vec![0u8; dst_w * dst_h * 4];
        for y in 0..dst_h {
            // Sampled at pixel centres. Sampling at the corner shifts the
            // whole image by half a source pixel, which is invisible on a
            // photograph and obvious on anything with a straight edge in it.
            let sy = origin_y + (y as f32 + 0.5) / scale;
            for x in 0..dst_w {
                let sx = origin_x + (x as f32 + 0.5) / scale;
                let [r, g, b] = self.sample(sx, sy);

                let i = (y * dst_w + x) * 4;
                out[i] = r as u8;
                out[i + 1] = g as u8;
                out[i + 2] = b as u8;
                // Opaque: this is the backmost thing in the scene, and a
                // wallpaper with holes in it shows the clear colour through.
                out[i + 3] = 0xFF;
            }
        }
        out
    }

    /// Bilinear sample, in `(R, G, B)` and unrounded.
    ///
    /// Bilinear and not nearest because the common case is a photograph on a
    /// panel that is not its size, and nearest-neighbour on a downscale is
    /// where the aliasing on a diagonal comes from.
    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 {
            return [0.0; 3];
        }

        // Half a pixel back, so `x0` is the sample to the left of the point
        // rather than the one containing it.
        let fx = (x - 0.5).clamp(0.0, (w - 1) as f32);
        let fy = (y - 0.5).clamp(0.0, (h - 1) as f32);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);

        let at = |px: usize, py: usize, c: usize| f32::from(self.pixels[(py * w + px) * 4 + c]);
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;

        let mut out = [0.0f32; 3];
        for (c, channel) in out.iter_mut().enumerate() {
            let top = lerp(at(x0, y0, c), at(x1, y0, c), tx);
            let bottom = lerp(at(x0, y1, c), at(x1, y1, c), tx);
            *channel = lerp(top, bottom, ty);
        }
        out
    }
}

/// The path of the wallpaper this machine has set.
fn installed_path() -> Option<PathBuf> {
    let entries = std::fs::read_dir(SET_DIR).ok()?;
    choose(
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            // Follows symlinks, so `set/wallpaper.jpg -> ../cliff.jpg` counts
            // and a directory called `wallpaper.d` does not.
            .filter(|path| path.is_file()),
    )
}

/// Pick the active wallpaper out of the names in [`SET_DIR`].
///
/// Split from [`installed_path`] so the rule is testable without a
/// filesystem, and sorted because `read_dir` yields whatever order the
/// filesystem feels like: two of these should not mean a desktop whose
/// wallpaper changes between boots. RavenLogin's greeter resolves the same
/// directory the same way, deliberately.
fn choose(entries: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = entries
        .filter(|path| path.file_stem().is_some_and(|stem| stem == SET_STEM))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Read and decode one image.
fn load(path: &std::path::Path) -> Result<Wallpaper, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat: {e}"))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "{size} bytes, past the {MAX_FILE_BYTES}-byte limit for a wallpaper"
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read: {e}"))?;
    decode(&bytes)
}

fn decode(bytes: &[u8]) -> Result<Wallpaper, String> {
    const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    const JPEG_MAGIC: &[u8] = &[0xFF, 0xD8, 0xFF];

    if bytes.starts_with(PNG_MAGIC) {
        decode_png(bytes)
    } else if bytes.starts_with(JPEG_MAGIC) {
        decode_jpeg(bytes)
    } else {
        Err("not a PNG or a JPEG".to_string())
    }
}

fn decode_png(bytes: &[u8]) -> Result<Wallpaper, String> {
    // A `Cursor`, because png wants `Read + Seek` and a slice is only `Read`.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Ask the decoder to normalise the awkward cases -- palettes, 1/2/4-bit
    // depths, 16-bit channels, missing alpha -- so the only outputs left to
    // handle are 8-bit RGBA and 8-bit grey+alpha.
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    decoder.set_limits(png::Limits {
        bytes: MAX_PIXELS * 4,
    });

    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("bad PNG header: {e}"))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    check_dimensions(width, height)?;

    let mut buffer = vec![
        0u8;
        reader
            .output_buffer_size()
            .ok_or_else(|| "PNG is too large".to_string())?
    ];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|e| format!("bad PNG data: {e}"))?;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::GrayscaleAlpha => 2,
        other => return Err(format!("unsupported PNG colour type {other:?}")),
    };

    let pixels = to_canvas_order(&buffer[..frame.buffer_size()], width, height, channels)?;
    Ok(Wallpaper {
        width,
        height,
        pixels,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Wallpaper, String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    // The caps are set before the header is read, so an oversized image is
    // refused by the decoder rather than allocated and then rejected.
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(MAX_DIMENSION)
        .set_max_height(MAX_DIMENSION);

    let mut decoder =
        zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|e| format!("bad JPEG header: {e}"))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| "the JPEG header carries no dimensions".to_string())?;
    let (width, height) = (
        u32::try_from(width).map_err(|_| "absurd JPEG width".to_string())?,
        u32::try_from(height).map_err(|_| "absurd JPEG height".to_string())?,
    );
    check_dimensions(width, height)?;

    let decoded = decoder
        .decode()
        .map_err(|e| format!("bad JPEG data: {e}"))?;

    // A greyscale JPEG comes back as one channel even having asked for RGB:
    // the requested colourspace is a request and not a conversion.
    let channels = match decoder.output_colorspace() {
        Some(ColorSpace::RGB) => 3,
        Some(ColorSpace::Luma) => 1,
        other => return Err(format!("unsupported JPEG colourspace {other:?}")),
    };

    let pixels = to_canvas_order(&decoded, width, height, channels)?;
    Ok(Wallpaper {
        width,
        height,
        pixels,
    })
}

fn check_dimensions(width: u32, height: u32) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Err(format!("{width}x{height} has no pixels"));
    }
    if w > MAX_DIMENSION || h > MAX_DIMENSION {
        return Err(format!(
            "{width}x{height} is past the {MAX_DIMENSION}-pixel limit on a side"
        ));
    }
    if w.saturating_mul(h) > MAX_PIXELS {
        return Err(format!(
            "{width}x{height} is past the {MAX_PIXELS}-pixel limit"
        ));
    }
    Ok(())
}

/// Expand `channels`-per-pixel input into the canvas's opaque RGBA.
///
/// Alpha is dropped rather than honoured: this is the backmost thing on the
/// screen, so a transparent wallpaper would show the clear colour and not
/// anything a user would call transparency.
fn to_canvas_order(
    src: &[u8],
    width: u32,
    height: u32,
    channels: usize,
) -> Result<Vec<u8>, String> {
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "absurd dimensions".to_string())?;
    if src.len() < count * channels {
        return Err(format!(
            "{} bytes for a {width}x{height} image with {channels} channels",
            src.len()
        ));
    }

    let mut out = vec![0u8; count * 4];
    for i in 0..count {
        let (from, to) = (i * channels, i * 4);
        let [r, g, b] = match channels {
            1 | 2 => [src[from]; 3],
            3 | 4 => [src[from], src[from + 1], src[from + 2]],
            other => return Err(format!("{other} channels per pixel")),
        };
        out[to] = r;
        out[to + 1] = g;
        out[to + 2] = b;
        out[to + 3] = 0xFF;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| Path::new(SET_DIR).join(n)).collect()
    }

    /// A 2x2: red, green / blue, white.
    fn checker() -> Wallpaper {
        #[rustfmt::skip]
        let pixels = vec![
            0xFF, 0x00, 0x00, 0xFF,   0x00, 0xFF, 0x00, 0xFF,
            0x00, 0x00, 0xFF, 0xFF,   0xFF, 0xFF, 0xFF, 0xFF,
        ];
        Wallpaper {
            width: 2,
            height: 2,
            pixels,
        }
    }

    #[test]
    fn any_extension_is_the_wallpaper() {
        for name in [
            "wallpaper.png",
            "wallpaper.jpg",
            "wallpaper.jpeg",
            "wallpaper",
        ] {
            assert_eq!(
                choose(paths(&[name]).into_iter()),
                Some(Path::new(SET_DIR).join(name)),
                "{name}"
            );
        }
    }

    #[test]
    fn other_names_are_not() {
        assert_eq!(choose(paths(&["cliff.jpg", "README"]).into_iter()), None);
        assert_eq!(choose(paths(&["wallpaper.old.png"]).into_iter()), None);
    }

    /// `read_dir` order is the filesystem's business, and a desktop that picks
    /// a different picture on alternate boots is a bug nobody can reproduce.
    #[test]
    fn two_wallpapers_resolve_the_same_way_every_time() {
        let forwards = choose(paths(&["wallpaper.png", "wallpaper.jpg"]).into_iter());
        let backwards = choose(paths(&["wallpaper.jpg", "wallpaper.png"]).into_iter());
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn an_empty_directory_has_no_wallpaper() {
        assert_eq!(choose(std::iter::empty()), None);
    }

    #[test]
    fn scaling_produces_exactly_the_requested_size() {
        for (w, h) in [(1, 1), (7, 3), (1920, 1080), (3, 400)] {
            assert_eq!(checker().scaled_to_cover(w, h).len(), w * h * 4, "{w}x{h}");
        }
    }

    #[test]
    fn scaling_leaves_every_pixel_opaque() {
        let out = checker().scaled_to_cover(9, 5);
        assert!(out.chunks_exact(4).all(|p| p[3] == 0xFF));
    }

    /// A destination the same shape as the source needs no crop at all, so
    /// every corner is the source pixel it came from, exactly.
    #[test]
    fn a_square_destination_keeps_the_corners() {
        let square = checker().scaled_to_cover(10, 10);
        let at = |x: usize, y: usize| {
            let i = (y * 10 + x) * 4;
            [square[i], square[i + 1], square[i + 2]]
        };
        assert_eq!(at(2, 2), [0xFF, 0x00, 0x00]);
        assert_eq!(at(7, 2), [0x00, 0xFF, 0x00]);
        assert_eq!(at(2, 7), [0x00, 0x00, 0xFF]);
        assert_eq!(at(7, 7), [0xFF, 0xFF, 0xFF]);
    }

    /// Cover crops the axis that does not fit rather than squashing it.
    ///
    /// A 2x2 over a 20x10 output scales by 10 both ways, which covers the
    /// width exactly and overshoots the height, so the middle band of the
    /// image is what survives. Two things follow, and both are asserted: the
    /// columns keep their colours, because nothing was squeezed horizontally,
    /// and no pixel is the source's pure red any more, because the top of the
    /// image was cropped away rather than compressed into view.
    #[test]
    fn cover_crops_rather_than_distorting() {
        let wide = checker().scaled_to_cover(20, 10);
        let at = |x: usize, y: usize| {
            let i = (y * 20 + x) * 4;
            [wide[i], wide[i + 1], wide[i + 2]]
        };
        let (top_left, top_right, bottom_left) = (at(0, 0), at(19, 0), at(0, 9));

        assert!(
            top_left[0] > 200 && top_left[1] == 0,
            "{top_left:?} is not red"
        );
        assert!(
            top_right[1] > 200 && top_right[0] < 60,
            "{top_right:?} is not green"
        );
        assert!(
            bottom_left[2] > 200 && bottom_left[0] < 60,
            "{bottom_left:?} is not blue"
        );
        assert!(top_left[0] < 0xFF, "{top_left:?} is the uncropped top row");
    }

    #[test]
    fn a_file_that_is_not_an_image_is_refused() {
        assert!(decode(b"#!/bin/sh\necho hello\n").is_err());
    }

    #[test]
    fn a_truncated_png_is_an_error_rather_than_a_panic() {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_truncated_jpeg_is_an_error_rather_than_a_panic() {
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]).is_err());
    }

    #[test]
    fn absurd_dimensions_are_refused() {
        assert!(check_dimensions(0, 100).is_err());
        assert!(check_dimensions(100, 0).is_err());
        assert!(check_dimensions(MAX_DIMENSION as u32 + 1, 4).is_err());
        assert!(check_dimensions(60_000, 60_000).is_err());
        assert!(check_dimensions(1920, 1080).is_ok());
    }

    #[test]
    fn a_short_buffer_is_an_error_rather_than_a_panic() {
        assert!(to_canvas_order(&[0x11, 0x22], 4, 4, 3).is_err());
    }

    #[test]
    fn grey_expands_to_three_channels() {
        assert_eq!(
            to_canvas_order(&[0x7F], 1, 1, 1).unwrap(),
            vec![0x7F, 0x7F, 0x7F, 0xFF]
        );
    }
}
