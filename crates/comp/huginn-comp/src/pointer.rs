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
    input::pointer::CursorIcon,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Transform},
};

use huginn_core::geometry::Rect;

use crate::state::{Huginn, has_buffer, layer_state, level_of};

/// The default cursor bitmap, loaded from the system xcursor theme.
#[derive(Debug)]
pub(crate) struct Cursor {
    pub buffer: MemoryRenderBuffer,
    /// Offset from the pointer position to the top-left of the bitmap, in
    /// logical pixels. The hotspot is the pixel that actually points at
    /// things — drawing the image at the pointer position without subtracting
    /// it puts the arrow's tip one icon down and to the right of where the
    /// user is aiming.
    pub hotspot: Point<i32, Logical>,
    /// The output density the bitmap was picked for. A backend compares this
    /// against the output's advertised scale to know when to load again.
    pub density: u32,
    /// The same cursor at the size it is drawn while being found, when the
    /// theme ships one. See [`Found`].
    pub found: Option<Found>,
}

/// A cursor's bitmap for while it is being found. See [`crate::shake`].
///
/// A theme ships its cursor at several sizes, each drawn for that size rather
/// than resampled from another, so a pointer to be shown three times as large
/// is far sharper taken from the theme at three times the size than stretched
/// from the ordinary bitmap on the GPU — which is what a 24-pixel arrow at 3×
/// looks like: blurred and stepped at once.
#[derive(Debug)]
pub(crate) struct Found {
    pub buffer: MemoryRenderBuffer,
    /// Offset from the pointer position to the top-left of the bitmap, in
    /// logical pixels, unrounded: a rounding error here is multiplied by the
    /// magnification, and the tip of a found pointer has to be exactly where
    /// the ordinary one's was.
    pub hotspot: Point<f64, Logical>,
    /// How many times larger than the ordinary bitmap this one draws. The
    /// theme need not ship exactly the size asked for, so the renderer scales
    /// this bitmap by `wanted / magnification`, which makes the pointer the
    /// size wanted whatever size the theme had.
    pub magnification: f64,
}

impl Cursor {
    /// The user's cursor for an output of `density`.
    ///
    /// `XCURSOR_THEME` and `XCURSOR_SIZE` are the conventional way users pick
    /// a cursor, so they are honoured rather than a setting invented. Logs
    /// when there is none: the pointer will be invisible over the background,
    /// which is worth a line in the log and not worth refusing to start.
    pub(crate) fn from_env(density: u32) -> Option<Self> {
        let cursor = Self::named(CursorIcon::Default, density);
        if cursor.is_none() {
            tracing::warn!(
                "no cursor theme found; the pointer will be invisible over the background"
            );
        }
        cursor
    }

    /// The user's cursor for `icon`, as named by a `cursor-shape-v1` client,
    /// on an output of `density`.
    ///
    /// Tries the icon's CSS name and then its legacy X11 aliases, since themes
    /// differ in which they ship. `None` when the theme has none of them; the
    /// caller falls back to the default arrow rather than showing nothing.
    pub(crate) fn named(icon: CursorIcon, density: u32) -> Option<Self> {
        let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_owned());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        std::iter::once(icon.name())
            .chain(icon.alt_names().iter().copied())
            .find_map(|name| Self::load(&theme, name, size, density))
    }

    /// Load `name` at roughly `size` logical pixels from `theme`, for an
    /// output of `density` pixels per logical one.
    ///
    /// The bitmap asked for is `size × density` pixels and the buffer is
    /// marked at that scale, so on a 2× output the 48-pixel image is drawn
    /// into 24 logical pixels: the same size on screen as at 1×, and sharp.
    /// A theme that ships no size that large gives a smaller cursor rather
    /// than a blurry one, which is the better of the two failures.
    ///
    /// The found pointer's bitmap is picked the same way at
    /// [`Huginn::FOUND_SIZE`] times the size, if the theme has anything
    /// larger than the ordinary one; if not, the renderer stretches the
    /// ordinary one, which is blurry but is still a large pointer.
    ///
    /// Returns `None` rather than failing the compositor: a missing cursor
    /// theme is a cosmetic problem, and refusing to start over it would be a
    /// far worse one.
    pub(crate) fn load(theme: &str, name: &str, size: u32, density: u32) -> Option<Self> {
        let density = density.max(1);
        let path = xcursor::CursorTheme::load(theme).load_icon(name)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .ok()?
            .read_to_end(&mut bytes)
            .ok()?;
        let images = xcursor::parser::parse_xcursor(&bytes)?;

        // Themes ship several sizes; take the closest to what was asked for
        // rather than assuming the first is sensible.
        let closest = |wanted: u32| {
            images
                .iter()
                .filter(|i| i.width > 0 && i.height > 0)
                .min_by_key(|i| i.size.abs_diff(wanted))
        };
        let image = closest(size * density)?;

        let wanted_found = ((size * density) as f32 * Huginn::FOUND_SIZE).round() as u32;
        let found = closest(wanted_found)
            // The same or a smaller bitmap is no sharper than stretching the
            // ordinary one, and costs a buffer.
            .filter(|big| big.width > image.width)
            .map(|big| Found {
                buffer: Self::buffer(big, density),
                hotspot: (
                    f64::from(big.xhot) / f64::from(density),
                    f64::from(big.yhot) / f64::from(density),
                )
                    .into(),
                magnification: f64::from(big.width) / f64::from(image.width),
            });

        Some(Self {
            buffer: Self::buffer(image, density),
            hotspot: ((image.xhot / density) as i32, (image.yhot / density) as i32).into(),
            density,
            found,
        })
    }

    /// A theme image as a buffer the renderer can draw, marked at `density`
    /// so that it covers `width / density` logical pixels.
    fn buffer(image: &xcursor::parser::Image, density: u32) -> MemoryRenderBuffer {
        // pixels_rgba is R,G,B,A in byte order, which is DRM's ABGR8888 —
        // little-endian A<<24|B<<16|G<<8|R. Reading this as Argb8888 gives a
        // blue-tinted cursor with the alpha channel in the wrong place.
        MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            density as i32,
            Transform::Normal,
            // No opaque regions: cursors are antialiased, so every pixel may
            // carry alpha. Claiming opacity would let the damage tracker skip
            // whatever is behind the cursor and leave a trail.
            None,
        )
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
        for (surface, rect, clip) in self.scene_surfaces() {
            // A client may still have its pre-resize buffer while its pane has
            // already been nested. Match hit testing to the compositor crop so
            // an invisible overflow cannot steal clicks from its sibling.
            if clip.is_some_and(|clip| {
                !clip.contains(huginn_core::geometry::Point::new(
                    point.x as i32,
                    point.y as i32,
                ))
            }) {
                continue;
            }
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
        // Every screen's workspace: the click may be on the other monitor.
        //
        // A minimized window keeps the geometry it had when it left the
        // layout, and that rectangle is now under the pane that grew to fill
        // it. It is not on screen, so it cannot be clicked: matching it would
        // focus a window nobody can see and ring the empty space it used to
        // occupy. Unmapped windows are skipped for the same reason.
        self.visible_window_ids().into_iter().rev().find(|id| {
            self.mapped.contains(id)
                && self
                    .space
                    .window(*id)
                    .is_some_and(|w| !w.is_minimized() && w.geometry.contains(p))
        })
    }

    /// The interactive layer surface under `point`, if there is one.
    ///
    /// Only surfaces that asked for the keyboard are candidates. A wallpaper
    /// spans the whole output and a status readout sits over the tiling area,
    /// so returning either would mean every click on the desktop moved focus
    /// away from the window the user was working in.
    ///
    /// Topmost wins, then most recently mapped, which is the same ordering
    /// [`huginn_core::layer::keyboard_focus`] applies to exclusive claims —
    /// clicking where two panels overlap should reach the one drawn on top.
    pub(crate) fn layer_under(&self, point: Point<f64, Logical>) -> Option<WlSurface> {
        let p = huginn_core::geometry::Point::new(point.x as i32, point.y as i32);
        self.all_layers()
            .into_iter()
            .enumerate()
            .filter_map(|(index, (surface, rect))| {
                // Unmapped surfaces are not clickable. One still holds its place
                // in the layout while it has nothing on screen, and letting a
                // click reach it would take focus away from the window the user
                // can actually see under it.
                if !has_buffer(surface.wl_surface()) {
                    return None;
                }
                let state = layer_state(surface)?;
                (state.interactivity != huginn_core::layer::Interactivity::None && rect.contains(p))
                    .then(|| (level_of(state.layer), index, surface.wl_surface().clone()))
            })
            .max_by_key(|(level, index, _)| (*level, *index))
            .map(|(_, _, surface)| surface)
    }

    /// Clamp the pointer to the output. Without this a relative motion event
    /// can walk the cursor off screen, where it is both invisible and unable to
    /// reach anything.
    ///
    /// With several screens the pointer may be anywhere on any of them, and
    /// where they touch it crosses freely. Off every screen, it goes to the
    /// nearest point on the nearest one, which is what lets it slide along a
    /// monitor's edge and round a corner where the two are not the same
    /// height.
    pub(crate) fn clamp_pointer(&self, point: Point<f64, Logical>) -> Point<f64, Logical> {
        let clamp_to = |area: Rect| -> Point<f64, Logical> {
            let max_x = f64::from(area.right() - 1).max(f64::from(area.x()));
            let max_y = f64::from(area.bottom() - 1).max(f64::from(area.y()));
            (
                point.x.clamp(f64::from(area.x()), max_x),
                point.y.clamp(f64::from(area.y()), max_y),
            )
                .into()
        };
        self.outputs()
            .iter()
            .map(|output| clamp_to(output.rect))
            .min_by(|a, b| {
                let da = (a.x - point.x).powi(2) + (a.y - point.y).powi(2);
                let db = (b.x - point.x).powi(2) + (b.y - point.y).powi(2);
                da.total_cmp(&db)
            })
            .unwrap_or(point)
    }
}
