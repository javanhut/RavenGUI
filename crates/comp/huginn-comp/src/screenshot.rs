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
//! udev backend already uses to feed a GPU-less display (`present_dumb`), and
//! the same one [`crate::record`] runs thirty times a second.
//!
//! Everything from [`Capture`] down is plain pixel work with no renderer in it,
//! so the cropping is unit-tested without a GPU.

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use smithay::{
    backend::renderer::{
        Bind, Color32F, ExportMem, Frame, Offscreen, Renderer,
        gles::{GlesMapping, GlesRenderer, GlesTexture},
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
const FORMAT: smithay::backend::allocator::Fourcc = smithay::backend::allocator::Fourcc::Abgr8888;

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
        .frame_size()
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
    let mut texture = offscreen_texture(renderer, size)?;
    let mapping = draw_offscreen(renderer, &mut texture, elements, size, scale, true)?;
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

/// A texture to capture a `size` screen into.
pub(crate) fn offscreen_texture(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
) -> Result<GlesTexture> {
    renderer
        .create_buffer(FORMAT, (size.w, size.h).into())
        .map_err(|e| anyhow::anyhow!("allocating the capture texture: {e}"))
}

/// Draw `elements` into `texture` and queue the read-back of what was drawn.
///
/// The returned mapping is a pixel buffer GL fills behind the draw; mapping it
/// is what hands the bytes over, and mapping blocks until the GPU has got that
/// far. `block` waits on the frame's fence here instead, which a one-off
/// screenshot can afford. A recording cannot: it maps the buffer on its next
/// tick, by which time the GPU long since has.
pub(crate) fn draw_offscreen(
    renderer: &mut GlesRenderer,
    texture: &mut GlesTexture,
    elements: &[HuginnElement],
    size: Size<i32, Physical>,
    scale: f64,
    block: bool,
) -> Result<GlesMapping> {
    draw_offscreen_over(renderer, texture, elements, size, scale, block, CLEAR)
}

/// [`draw_offscreen`] over a background of `clear` rather than the desktop's.
///
/// A capture of one window clears to transparent, so its rounded corners and
/// shadow come out as the window drew them rather than on a slab of the
/// empty-desktop colour.
pub(crate) fn draw_offscreen_over(
    renderer: &mut GlesRenderer,
    texture: &mut GlesTexture,
    elements: &[HuginnElement],
    size: Size<i32, Physical>,
    scale: f64,
    block: bool,
    clear: Color32F,
) -> Result<GlesMapping> {
    let damage = [Rectangle::from_size(size)];
    let mut framebuffer = renderer
        .bind(texture)
        .map_err(|e| anyhow::anyhow!("binding the capture texture: {e}"))?;
    {
        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Normal)
            .map_err(|e| anyhow::anyhow!("starting the capture frame: {e}"))?;
        frame
            .clear(clear, &damage)
            .map_err(|e| anyhow::anyhow!("clearing the capture: {e}"))?;
        draw_render_elements::<GlesRenderer, _, _>(
            &mut frame,
            Scale::from(scale),
            elements,
            &damage,
        )
        .map_err(|e| anyhow::anyhow!("drawing the capture: {e}"))?;
        let sync = frame
            .finish()
            .map_err(|e| anyhow::anyhow!("finishing the capture: {e}"))?;
        if block {
            let _ = sync.wait();
        }
    }

    renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((size.w, size.h).into()),
            FORMAT,
        )
        .map_err(|e| anyhow::anyhow!("reading the capture back: {e}"))
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
        let dir = crate::userdirs::user_dir("XDG_PICTURES_DIR", "Pictures")
            .map(|base| base.join("Screenshots"))
            .context("no directory to save a screenshot in (no HOME, no XDG_PICTURES_DIR)")?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let stem = crate::userdirs::timestamp("Screenshot");
        let path = crate::userdirs::unique_path(&dir, &stem, "png");
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            image
                .cropped(Rectangle::new((0, 0).into(), (0, 0).into()))
                .is_none()
        );
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
