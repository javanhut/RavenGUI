//! Scene assembly, shared by both backends.
//!
//! Both backends paint the same thing; only the surface they paint onto
//! differs. Keeping assembly here means a stacking-order fix cannot land in one
//! backend and not the other.
//!
//! # Scale
//!
//! Every position here is logical and every buffer is physical, and the scale
//! that converts between them is the output's *advertised* integer scale — the
//! same whole number clients were told, so a 2x client buffer lands on 2x worth
//! of pixels and nothing is resampled on the way. See `huginn_core::scale` for
//! why it is never a fraction.

use std::sync::Mutex;

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer,
        element::{
            Kind, memory::MemoryRenderBufferRenderElement, render_elements,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    utils::{Logical, Point},
    wayland::compositor::with_states,
};

use crate::pointer::Cursor;
use crate::state::{Huginn, SceneItem};

render_elements! {
    /// Everything Huginn can draw.
    pub(crate) HuginnElement<R> where R: ImportAll + ImportMem;
    /// A client surface: window, panel, wallpaper.
    Surface = WaylandSurfaceRenderElement<R>,
    /// The cursor, when no client has supplied its own.
    Cursor = MemoryRenderBufferRenderElement<R>,
    /// One edge of the ring around the focused window.
    Ring = SolidColorRenderElement,
}

/// Build the full scene, cursor included, front to back.
pub(crate) fn elements<R>(
    renderer: &mut R,
    state: &Huginn,
    fallback_cursor: Option<&Cursor>,
) -> Vec<HuginnElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Send + 'static,
{
    // The output's advertised integer scale, as an f64 for the element APIs.
    // Whole numbers only, so this conversion is exact and no position can land
    // between pixels on the way from logical to physical.
    let scale = f64::from(state.scale.advertised);

    let mut out: Vec<HuginnElement<R>> = Vec::new();

    // The cursor goes first because the scene is painted front to back, and
    // nothing is ever meant to occlude the pointer.
    match &state.cursor_status {
        // The client drew its own cursor. Its hotspot lives in the surface's
        // own state, and ignoring it puts the arrow's tip in the wrong place.
        CursorImageStatus::Surface(surface) => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .map(|attrs| attrs.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let position: Point<i32, Logical> =
                state.pointer_location.to_i32_round::<i32>() - hotspot;
            out.extend(
                render_elements_from_surface_tree(
                    renderer,
                    surface,
                    position.to_physical(scale as i32),
                    scale,
                    1.0,
                    Kind::Cursor,
                )
                .into_iter()
                .map(HuginnElement::Surface),
            );
        }
        // Nothing has claimed the cursor, so draw the theme's default.
        CursorImageStatus::Named(_) => {
            if let Some(cursor) = fallback_cursor {
                let position: Point<f64, Logical> = (
                    state.pointer_location.x - f64::from(cursor.hotspot.x),
                    state.pointer_location.y - f64::from(cursor.hotspot.y),
                )
                    .into();
                if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    position.to_physical(1.0),
                    &cursor.buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    out.push(HuginnElement::Cursor(element));
                }
            }
        }
        // A client asked for no cursor at all, e.g. while typing or in a game.
        CursorImageStatus::Hidden => {}
    }

    for item in state.scene() {
        match item {
            SceneItem::Surface(surface, rect) => out.extend(
                render_elements_from_surface_tree(
                    renderer,
                    &surface,
                    Point::<i32, Logical>::from((rect.x(), rect.y())).to_physical(scale as i32),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(HuginnElement::Surface),
            ),
            SceneItem::Overlay(buffer, rect, alpha) => {
                // `size` scales the buffer and `alpha` fades it, both per
                // frame and both free — which is what lets the launcher grow
                // out of the dock icon without being composed again at every
                // intermediate size. Re-shaping its text sixty times a second
                // would cost more than the animation is worth, and the glyphs
                // would shimmer as they re-hinted at each one.
                if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    Point::<f64, Logical>::from((f64::from(rect.x()), f64::from(rect.y())))
                        .to_physical(scale),
                    buffer,
                    Some(alpha),
                    None,
                    Some((rect.w(), rect.h()).into()),
                    Kind::Unspecified,
                ) {
                    out.push(HuginnElement::Cursor(element));
                }
            }
            SceneItem::Ring(buffer, rect) => {
                out.push(HuginnElement::Ring(SolidColorRenderElement::from_buffer(
                    buffer,
                    Point::<i32, Logical>::from((rect.x(), rect.y())).to_physical(scale as i32),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )));
            }
        }
    }

    out
}
