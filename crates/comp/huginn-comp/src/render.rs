//! Scene assembly, shared by both backends.
//!
//! Both backends paint the same thing; only the surface they paint onto
//! differs. Keeping assembly here means a stacking-order fix cannot land in one
//! backend and not the other.

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
                    // Scale is 1.0 throughout for now, so logical and physical
                    // coordinates coincide; this becomes a real conversion when
                    // fractional scaling lands.
                    position.to_physical(1),
                    1.0,
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
                    surface,
                    (rect.x(), rect.y()),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(HuginnElement::Surface),
            ),
            SceneItem::Ring(buffer, rect) => {
                out.push(HuginnElement::Ring(SolidColorRenderElement::from_buffer(
                    buffer,
                    (rect.x(), rect.y()),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                )));
            }
        }
    }

    out
}
