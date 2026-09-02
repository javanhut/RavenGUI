//! Screen capture, drawn by the compositor itself.
//!
//! # Why this is in the compositor
//!
//! On Wayland a client can only read the screen if the compositor hands it the
//! pixels through a capture protocol — `wlr-screencopy` or
//! `ext-image-copy-capture-v1`. Huginn advertises neither (see
//! `docs/protocols.md`), and it holds DRM master alone, so nothing else on the
//! session can reach the framebuffer. A screenshot therefore cannot be a
//! separate program the way it is on most desktops; it is a compositor feature,
//! bound to `Print`, exactly as the launcher and the help overlay are.
//!
//! That is the same reasoning the shell is drawn in-process: the thing that
//! must capture what is on screen is the one process that already has it.
//!
//! # How it works
//!
//! The compositor renders the scene a second time into an offscreen texture at
//! the output's physical size, reads it back with [`ExportMem`], and writes a
//! PNG. The scene is assembled by [`crate::render::capture_elements`], which is
//! the ordinary scene minus the pointer — a screenshot of the desktop should
//! not have a cursor stamped into it. The read-back is the same
//! `create_buffer` → `bind` → draw → `copy_framebuffer` → `map_texture` path the
//! udev backend already uses to feed a GPU-less display (`present_dumb`).
//!
//! Everything from [`Capture`] down is plain pixel work with no renderer in it,
//! so the cropping, the path policy and the timestamp are unit-tested without a
//! GPU.

use std::ffi::OsStr;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use smithay::{
    backend::renderer::{
        Bind, Color32F, ExportMem, Frame, Offscreen, Renderer,
        gles::{GlesRenderer, GlesTexture},
        utils::draw_render_elements,
    },
    utils::{Physical, Rectangle, Scale, Size, Transform},
};

use crate::render::{HuginnElement, capture_elements};
use crate::state::Huginn;
use huginn_core::geometry::Rect;

/// Which area a screenshot covers.
///
/// The three the `Print` key resolves to, from its modifiers. [`Shot::Region`]
/// does not capture on its own — it arms an interactive selection that the
/// compositor takes on the pointer release. See [`crate::backend::keymap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shot {
    /// The focused screen, whole.
    Screen,
    /// The focused window's rectangle.
    Window,
    /// A rectangle the user drags out.
    Region,
}

/// The clear colour behind the desktop, matching both backends' `CLEAR`. It
/// shows only where nothing else is drawn, which on a desktop with a wallpaper
/// is nowhere, but a capture must not depend on that.
const CLEAR: Color32F = Color32F::new(0.06, 0.06, 0.09, 1.0);

/// Read-back format. DRM `ABGR8888` is `R, G, B, A` in memory on a
/// little-endian host, which is exactly PNG's byte order, so the mapping is
/// copied out with no channel shuffle. The offscreen texture is created in the
/// same format, an `RGBA8` GLES texture, which `GlesRenderer` can render into.
const FORMAT: smithay::backend::allocator::Fourcc =
    smithay::backend::allocator::Fourcc::Abgr8888;

/// Capture an output, optionally cropped to a rectangle within it, and save it.
///
/// `crop` is in the desktop's global logical pixels, like everything
/// `huginn-core` deals in; it is converted to the output's physical pixels here.
/// Returns where the file was written.
pub(crate) fn capture(
    renderer: &mut GlesRenderer,
    state: &Huginn,
    output: usize,
    crop: Option<Rect>,
) -> Result<PathBuf> {
    let info = state
        .outputs()
        .get(output)
        .context("capturing a screen that is not connected")?;
    let view = info.rect;
    let scale = info.scale.fractional();
    let size = info
        .output
        .as_ref()
        .and_then(|output| output.current_mode())
        .map(|mode| mode.size)
        .context("the screen has no mode, so nothing to capture")?;

    let elements = capture_elements(renderer, state, view, scale);
    let full = grab(renderer, &elements, size, scale)?;

    let image = match crop {
        Some(rect) => {
            let region = to_physical(rect, view, scale, size);
            full.cropped(region)
                .context("the capture area is empty or off the screen")?
        }
        None => full,
    };
    image.save()
}

/// Render `elements` into an offscreen texture the size of the screen and read
/// the pixels back as 8-bit RGBA, top row first.
fn grab(
    renderer: &mut GlesRenderer,
    elements: &[HuginnElement],
    size: Size<i32, Physical>,
    scale: f64,
) -> Result<Capture> {
    let damage = [Rectangle::from_size(size)];
    let mut texture: GlesTexture = renderer
        .create_buffer(FORMAT, (size.w, size.h).into())
        .map_err(|e| anyhow::anyhow!("allocating the capture texture: {e}"))?;
    let mut framebuffer = renderer
        .bind(&mut texture)
        .map_err(|e| anyhow::anyhow!("binding the capture texture: {e}"))?;
    {
        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Normal)
            .map_err(|e| anyhow::anyhow!("starting the capture frame: {e}"))?;
        frame
            .clear(CLEAR, &damage)
            .map_err(|e| anyhow::anyhow!("clearing the capture: {e}"))?;
        draw_render_elements::<GlesRenderer, _, _>(
            &mut frame,
            Scale::from(scale),
            elements,
            &damage,
        )
        .map_err(|e| anyhow::anyhow!("drawing the capture: {e}"))?;
        // Block on the fence: the read-back below must see a finished frame, not
        // one still on the GPU.
        let sync = frame
            .finish()
            .map_err(|e| anyhow::anyhow!("finishing the capture: {e}"))?;
        let _ = sync.wait();
    }

    let mapping = renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((size.w, size.h).into()),
            FORMAT,
        )
        .map_err(|e| anyhow::anyhow!("reading the capture back: {e}"))?;
    let rgba = renderer
        .map_texture(&mapping)
        .map_err(|e| anyhow::anyhow!("mapping the capture: {e}"))?
        .to_vec();

    Ok(Capture {
        width: size.w,
        height: size.h,
        rgba,
    })
}

/// A window/region rectangle in global logical pixels, turned into the
/// captured image's own physical pixels and clamped to it.
fn to_physical(
    rect: Rect,
    view: Rect,
    scale: f64,
    size: Size<i32, Physical>,
) -> Rectangle<i32, Physical> {
    // Into the screen's own space, then into its physical pixels.
    let x = ((f64::from(rect.x() - view.x())) * scale).round() as i32;
    let y = ((f64::from(rect.y() - view.y())) * scale).round() as i32;
    let w = ((f64::from(rect.w())) * scale).round() as i32;
    let h = ((f64::from(rect.h())) * scale).round() as i32;
    // Clamp to the image: a window can hang off the edge of its screen, and a
    // read outside the buffer is not a screenshot, it is a crash.
    let x0 = x.clamp(0, size.w);
    let y0 = y.clamp(0, size.h);
    let x1 = (x + w).clamp(0, size.w);
    let y1 = (y + h).clamp(0, size.h);
    Rectangle::new((x0, y0).into(), ((x1 - x0).max(0), (y1 - y0).max(0)).into())
}

/// A captured image, 8-bit RGBA, top row first.
pub(crate) struct Capture {
    width: i32,
    height: i32,
    rgba: Vec<u8>,
}

impl Capture {
    /// Cut the image down to `region`, in its own physical pixels. `None` if the
    /// region has no area — a zero-drag selection, or a crop clamped away to
    /// nothing.
    fn cropped(&self, region: Rectangle<i32, Physical>) -> Option<Capture> {
        let (rx, ry) = (region.loc.x, region.loc.y);
        let (rw, rh) = (region.size.w, region.size.h);
        if rw <= 0 || rh <= 0 {
            return None;
        }
        let mut out = Vec::with_capacity((rw * rh * 4) as usize);
        for row in 0..rh {
            let src_y = ry + row;
            let start = ((src_y * self.width + rx) * 4) as usize;
            let end = start + (rw * 4) as usize;
            out.extend_from_slice(&self.rgba[start..end]);
        }
        Some(Capture {
            width: rw,
            height: rh,
            rgba: out,
        })
    }

    /// Write the image to the screenshots directory and return its path.
    fn save(&self) -> Result<PathBuf> {
        let dir = screenshot_dir()
            .context("no directory to save a screenshot in (no HOME, no XDG_PICTURES_DIR)")?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = unique_path(&dir, &timestamp());
        encode_png(&path, self.width, self.height, &self.rgba)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

/// Encode RGBA pixels as a PNG.
fn encode_png(path: &Path, width: i32, height: i32, rgba: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(())
}

/// `<pictures>/Screenshots`, the directory shots are written to.
///
/// The pictures directory is the one the user-dirs specification names —
/// `$XDG_PICTURES_DIR` if it is exported, otherwise the value in
/// `user-dirs.dirs`, otherwise `~/Pictures`. `None` only when there is no `HOME`
/// and nothing to anchor a relative path to, which is the one case there is
/// nowhere sensible to put a file.
fn screenshot_dir() -> Option<PathBuf> {
    let user_dirs = read_user_dirs();
    pictures_base(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_PICTURES_DIR").as_deref(),
        user_dirs.as_deref(),
    )
    .map(|base| base.join("Screenshots"))
}

/// The pictures directory, from the environment and the `user-dirs.dirs`
/// contents. Split out with the inputs passed in so the policy is testable
/// without touching the real environment.
fn pictures_base(
    home: Option<&OsStr>,
    xdg_pictures: Option<&OsStr>,
    user_dirs: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = xdg_pictures.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = home.filter(|value| !value.is_empty())?;
    let home = Path::new(home);
    if let Some(relative) = user_dirs.and_then(parse_pictures_dir) {
        return Some(expand_home(&relative, home));
    }
    Some(home.join("Pictures"))
}

/// The contents of `user-dirs.dirs`, from `$XDG_CONFIG_HOME` or `~/.config`.
fn read_user_dirs() -> Option<String> {
    let path = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(config) => PathBuf::from(config).join("user-dirs.dirs"),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config/user-dirs.dirs"),
    };
    std::fs::read_to_string(path).ok()
}

/// The `XDG_PICTURES_DIR="..."` value from a `user-dirs.dirs` file, unquoted.
///
/// The format is shell assignments; the value is double-quoted and usually
/// begins with `$HOME`. Comments and other keys are skipped.
fn parse_pictures_dir(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("XDG_PICTURES_DIR=") else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if value.is_empty() {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

/// Expand a leading `$HOME` (or `~`) in a user-dirs value against `home`.
fn expand_home(value: &str, home: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("$HOME/") {
        home.join(rest)
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if value == "$HOME" || value == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(value)
    }
}

/// A filename for a shot taken now, unique within `dir`.
///
/// Timestamped to the second; a second shot in the same second gets a `-2`
/// suffix rather than overwriting the first.
fn unique_path(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(format!("{base}.png"));
    if !first.exists() {
        return first;
    }
    for n in 2.. {
        let candidate = dir.join(format!("{base}-{n}.png"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the integers do not run out")
}

/// A `Screenshot-YYYY-MM-DD-HHMMSS` stem, in UTC.
///
/// UTC rather than local time because turning a Unix timestamp into local time
/// needs the zone database and a C library call, and a compositor that forbids
/// unsafe is not going to reach for `localtime` to name a file. The name is for
/// telling two shots apart, which UTC does exactly as well.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_utc(secs);
    format!("Screenshot-{y:04}-{mo:02}-{d:02}-{h:02}{mi:02}{s:02}")
}

/// Broken-down UTC time from a Unix timestamp: `(year, month, day, hour, min,
/// sec)`. Howard Hinnant's `civil_from_days`, which is exact and needs no zone
/// data.
fn civil_utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn an_exported_pictures_dir_wins() {
        let base = pictures_base(
            Some(OsStr::new("/home/person")),
            Some(OsStr::new("/photos")),
            Some("XDG_PICTURES_DIR=\"$HOME/Pictures\"\n"),
        );
        assert_eq!(base, Some(PathBuf::from("/photos")));
    }

    #[test]
    fn an_empty_exported_pictures_dir_is_ignored() {
        // A shell that exported the variable blank has said nothing, not "put it
        // at the filesystem root".
        let base = pictures_base(Some(OsStr::new("/home/person")), Some(OsStr::new("")), None);
        assert_eq!(base, Some(PathBuf::from("/home/person/Pictures")));
    }

    #[test]
    fn user_dirs_is_consulted_and_home_expanded() {
        let base = pictures_base(
            Some(OsStr::new("/home/person")),
            None,
            Some("# generated\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\n"),
        );
        assert_eq!(base, Some(PathBuf::from("/home/person/Bilder")));
    }

    #[test]
    fn without_user_dirs_it_falls_back_to_pictures() {
        let base = pictures_base(Some(OsStr::new("/home/person")), None, None);
        assert_eq!(base, Some(PathBuf::from("/home/person/Pictures")));
    }

    #[test]
    fn with_no_home_there_is_nowhere_to_put_it() {
        assert_eq!(pictures_base(None, None, None), None);
        // ...unless an absolute pictures dir was exported, which needs no home.
        assert_eq!(
            pictures_base(None, Some(OsStr::new("/shots")), None),
            Some(PathBuf::from("/shots"))
        );
    }

    #[test]
    fn an_absolute_user_dirs_value_is_left_alone() {
        assert_eq!(
            expand_home("/mnt/pics", Path::new("/home/person")),
            PathBuf::from("/mnt/pics")
        );
    }

    #[test]
    fn known_timestamps_break_down_correctly() {
        // 2023-11-14T22:13:20Z
        assert_eq!(civil_utc(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        // The epoch itself.
        assert_eq!(civil_utc(0), (1970, 1, 1, 0, 0, 0));
        // A leap day: 2024-02-29T12:00:00Z.
        assert_eq!(civil_utc(1_709_208_000), (2024, 2, 29, 12, 0, 0));
    }

    #[test]
    fn cropping_takes_the_right_rows() {
        // A 4x3 image where each pixel's red channel is its row so a crop is
        // easy to check. Green holds the column.
        let (w, h) = (4, 3);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[y as u8, x as u8, 0, 255]);
            }
        }
        let image = Capture {
            width: w,
            height: h,
            rgba,
        };
        // Rows 1..3, columns 1..3.
        let region = Rectangle::new((1, 1).into(), (2, 2).into());
        let cropped = image.cropped(region).expect("non-empty crop");
        assert_eq!(cropped.width, 2);
        assert_eq!(cropped.height, 2);
        // Top-left of the crop is the original (col 1, row 1).
        assert_eq!(&cropped.rgba[0..4], &[1, 1, 0, 255]);
        // Bottom-right is (col 2, row 2).
        assert_eq!(&cropped.rgba[cropped.rgba.len() - 4..], &[2, 2, 0, 255]);
    }

    #[test]
    fn an_empty_region_crops_to_nothing() {
        let image = Capture {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
        };
        assert!(image.cropped(Rectangle::new((0, 0).into(), (0, 0).into())).is_none());
    }

    #[test]
    fn a_window_off_the_screen_edge_clamps_into_the_image() {
        // A 1000x1000 physical screen at 1x, a window hanging off the right.
        let view = Rect::from_xywh(0, 0, 1000, 1000);
        let rect = Rect::from_xywh(900, 100, 400, 200);
        let region = to_physical(rect, view, 1.0, Size::from((1000, 1000)));
        assert_eq!(region.loc.x, 900);
        assert_eq!(region.size.w, 100, "clamped to the screen's right edge");
        assert_eq!(region.size.h, 200);
    }

    #[test]
    fn physical_conversion_applies_the_fractional_scale() {
        // A region on a 2x-ish screen is twice as many device pixels across.
        let view = Rect::from_xywh(0, 0, 1280, 720);
        let rect = Rect::from_xywh(100, 50, 200, 100);
        let region = to_physical(rect, view, 2.0, Size::from((2560, 1440)));
        assert_eq!((region.loc.x, region.loc.y), (200, 100));
        assert_eq!((region.size.w, region.size.h), (400, 200));
    }
}
