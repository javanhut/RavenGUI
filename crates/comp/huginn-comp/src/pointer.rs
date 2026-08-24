//! Pointer support: what is under the cursor, and what the cursor looks like.
//!
//! A compositor that advertises a pointer must also draw one. Clients set their
//! own cursor whenever the pointer enters them, but over the compositor's own
//! background nothing has been set, and without a fallback the pointer is
//! invisible exactly where the user is most likely to be looking for it. So the
//! system xcursor theme is loaded once at startup and used as the default.

use std::io::Read;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    desktop::{WindowSurfaceType, utils::under_from_surface_tree},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Transform},
};

use huginn_core::geometry::Rect;

use crate::state::Huginn;

/// The default cursor bitmap, loaded from the system xcursor theme.
#[derive(Debug)]
pub(crate) struct Cursor {
    pub buffer: MemoryRenderBuffer,
    /// Offset from the pointer position to the top-left of the bitmap. The
    /// hotspot is the pixel that actually points at things — drawing the image
    /// at the pointer position without subtracting it puts the arrow's tip one
    /// icon down and to the right of where the user is aiming.
    pub hotspot: Point<i32, Logical>,
}

impl Cursor {
    /// Load `name` at roughly `size` pixels from `theme`.
    ///
    /// Returns `None` rather than failing the compositor: a missing cursor
    /// theme is a cosmetic problem, and refusing to start over it would be a
    /// far worse one.
    pub(crate) fn load(theme: &str, name: &str, size: u32) -> Option<Self> {
        let path = xcursor::CursorTheme::load(theme).load_icon(name)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&path).ok()?.read_to_end(&mut bytes).ok()?;
        let images = xcursor::parser::parse_xcursor(&bytes)?;

        // Themes ship several sizes; take the closest to what was asked for
        // rather than assuming the first is sensible.
        let image = images
            .iter()
            .min_by_key(|i| i.size.abs_diff(size))
            .filter(|i| i.width > 0 && i.height > 0)?;

        // pixels_rgba is R,G,B,A in byte order, which is DRM's ABGR8888 —
        // little-endian A<<24|B<<16|G<<8|R. Reading this as Argb8888 gives a
        // blue-tinted cursor with the alpha channel in the wrong place.
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            // No opaque regions: cursors are antialiased, so every pixel may
            // carry alpha. Claiming opacity would let the damage tracker skip
            // whatever is behind the cursor and leave a trail.
            None,
        );

        Some(Self {
            buffer,
            hotspot: (image.xhot as i32, image.yhot as i32).into(),
        })
    }
}

impl Huginn {
    /// The topmost surface under `point`, with its absolute position.
    ///
    /// Walks the scene in the same front-to-back order it is painted in, so
    /// what you click is always what you can see. Descends into subsurfaces and
    /// honours input regions, which is why a client can have a transparent
    /// border that clicks fall straight through.
    ///
    /// `point` and the surface origin both go in as global coordinates:
    /// `under_from_surface_tree` subtracts the one from the other itself, and
    /// relativising the point first subtracts the origin twice. That reads as
    /// clicks landing a window's own position away from the pointer — harmless
    /// for a surface at the screen origin like the panel, and enough to miss
    /// the surface entirely for one tiled off to the right.
    pub(crate) fn surface_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<i32, Logical>)> {
        for (surface, rect) in self.scene_surfaces() {
            let origin: Point<i32, Logical> = (rect.x(), rect.y()).into();
            if let Some(found) =
                under_from_surface_tree(&surface, point, origin, WindowSurfaceType::ALL)
            {
                return Some(found);
            }
        }
        None
    }

    /// The window whose geometry contains `point`, if any.
    ///
    /// Used for click-to-focus, which cares about the window rather than the
    /// particular subsurface that was hit.
    pub(crate) fn window_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<huginn_core::window::WindowId> {
        let p = huginn_core::geometry::Point::new(point.x as i32, point.y as i32);
        self.space
            .active_workspace()
            .windows()
            .iter()
            .rev()
            .find(|id| {
                self.space
                    .window(**id)
                    .is_some_and(|w| w.geometry.contains(p))
            })
            .copied()
    }

    /// Clamp the pointer to the output. Without this a relative motion event
    /// can walk the cursor off screen, where it is both invisible and unable to
    /// reach anything.
    pub(crate) fn clamp_pointer(&self, point: Point<f64, Logical>) -> Point<f64, Logical> {
        let area: Rect = self.output_area();
        let max_x = f64::from(area.right() - 1).max(0.0);
        let max_y = f64::from(area.bottom() - 1).max(0.0);
        (
            point.x.clamp(f64::from(area.x()), max_x),
            point.y.clamp(f64::from(area.y()), max_y),
        )
            .into()
    }
}
