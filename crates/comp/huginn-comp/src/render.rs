//! Scene assembly, shared by both backends.
//!
//! Both backends paint the same thing; only the surface they paint onto
//! differs. Keeping assembly here means a stacking-order fix cannot land in one
//! backend and not the other.
//!
//! # Scale
//!
//! Every position here is logical and every buffer is physical, and the scale
//! that converts between them is the output's *fractional* scale — what the
//! desktop is composed at, which is the advertised integer on a 1× or clean 2×
//! panel and something like 1.5 on a 4K 27". Clients only ever hear the
//! integer; the fraction is applied here, once per surface, as it is drawn.
//! Each element is snapped to whole physical pixels (`precise_round`), and
//! neighbours round the same coordinate the same way, so tiles meet without
//! seams. See `huginn_core::scale`.
//!
//! Both backends draw these elements with the same scale — udev through the
//! `Output` the `DrmCompositor` reads, winit by passing it explicitly — and
//! the blur pass has to as well. An element's size is worked out from the
//! scale it is *drawn* with, so a mismatch does not shift it, it resizes it.

use std::sync::Mutex;

use smithay::{
    backend::renderer::{
        element::{
            Kind,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            utils::{CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement},
        },
        gles::{GlesRenderer, element::TextureShaderElement},
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    utils::{Logical, Point, Rectangle},
    wayland::compositor::with_states,
};

use crate::pointer::Cursor;
use crate::state::{Huginn, SceneItem};

render_elements! {
    /// Everything Huginn can draw.
    ///
    /// Concrete over `GlesRenderer` rather than generic. Both backends use it —
    /// winit through `WinitGraphicsBackend<GlesRenderer>` and udev directly —
    /// so the generic bought nothing, and it cost the ability to hold a
    /// [`TextureShaderElement`], which exists only for GLES.
    pub(crate) HuginnElement<=GlesRenderer>;
    /// A client surface: window, panel, wallpaper.
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    /// A client surface transformed as part of a workspace preview card.
    Workspace = RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    /// A client surface drawn 1:1 and cropped to a rectangle still on its
    /// way somewhere: a tile mid-resize.
    Clipped = CropRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    WorkspaceCard = RelocateRenderElement<RescaleRenderElement<SolidColorRenderElement>>,
    /// The cursor, when no client has supplied its own.
    Cursor = MemoryRenderBufferRenderElement<GlesRenderer>,
    /// One edge of the ring around the focused window.
    Ring = SolidColorRenderElement,
    /// The desktop, already drawn and blurred into a texture, cropped to the
    /// panel that wants it. See [`blur_element`].
    Blur = CropRenderElement<TextureShaderElement>,
}

/// The blurred desktop, cut down to `rect` — the logical region under a
/// panel — so it goes on top of the sharp desktop and under the panel.
///
/// The blur texture is the whole output; the crop is what turns "the desktop
/// is blurred" into "the desktop is blurred *behind the launcher*". The rest
/// of the screen keeps its sharp pixels from the ordinary elements drawn
/// beneath, which is also why the blur can go in the middle of a front-to-back
/// list rather than replacing the back of it.
///
/// `None` if the crop meets nothing, which happens when the rectangle is off
/// the output entirely; a blur that draws nothing is not worth an element.
pub(crate) fn blur_element(
    blurred: TextureShaderElement,
    rect: huginn_core::geometry::Rect,
    scale: f64,
) -> Option<HuginnElement> {
    let crop =
        Rectangle::<i32, Logical>::new((rect.x(), rect.y()).into(), (rect.w(), rect.h()).into())
            .to_physical_precise_round(scale);
    CropRenderElement::from_element(blurred, scale, crop).map(HuginnElement::Blur)
}

/// The scene split at the blur boundary: what goes over a blurred desktop,
/// and what gets blurred.
///
/// The cursor is always in the first group — blurring the pointer would be
/// blurring the one thing the user is aiming with.
pub(crate) fn elements_split(
    renderer: &mut GlesRenderer,
    state: &Huginn,
    fallback_cursor: Option<&Cursor>,
) -> (Vec<HuginnElement>, Vec<HuginnElement>) {
    let all = elements(renderer, state, fallback_cursor);
    // `elements` puts the cursor in front of everything `scene` produced, so
    // the boundary is that plus the panels above it.
    let cursor = all.len() - state.scene().len();
    let above = cursor + state.blur_boundary();
    let mut front = all;
    let back = front.split_off(above.min(front.len()));
    (front, back)
}

/// Build the full scene, cursor included, front to back.
pub(crate) fn elements(
    renderer: &mut GlesRenderer,
    state: &Huginn,
    fallback_cursor: Option<&Cursor>,
) -> Vec<HuginnElement> {
    // The scale the desktop is composed at. See the module docs.
    let scale = state.scale.fractional();

    let mut out: Vec<HuginnElement> = Vec::new();

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
                    position.to_physical_precise_round::<f64, i32>(scale),
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
                    position.to_physical(scale),
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
                    Point::<i32, Logical>::from((rect.x(), rect.y()))
                        .to_physical_precise_round::<f64, i32>(scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(HuginnElement::Surface),
            ),
            SceneItem::Clipped(surface, rect, clip, alpha) => {
                // Cropped, not scaled: the content stays at its natural size
                // and the moving edge cuts it off. See `crate::motion`.
                let crop = Rectangle::<i32, Logical>::new(
                    (clip.x(), clip.y()).into(),
                    (clip.w(), clip.h()).into(),
                )
                .to_physical_precise_round(scale);
                out.extend(
                    render_elements_from_surface_tree(
                        renderer,
                        &surface,
                        Point::<i32, Logical>::from((rect.x(), rect.y()))
                            .to_physical_precise_round::<f64, i32>(scale),
                        scale,
                        alpha,
                        Kind::Unspecified,
                    )
                    .into_iter()
                    .filter_map(|element| CropRenderElement::from_element(element, scale, crop))
                    .map(HuginnElement::Clipped),
                );
            }
            SceneItem::WorkspaceSurface(surface, rect, transform)
            | SceneItem::Preview(surface, rect, transform) => {
                let origin = Point::<i32, Logical>::from((rect.x(), rect.y()))
                    .to_physical_precise_round::<f64, i32>(scale);
                let offset = Point::<f64, Logical>::from((transform.offset_x, transform.offset_y))
                    .to_physical_precise_round::<f64, i32>(scale);
                out.extend(
                    render_elements_from_surface_tree(
                        renderer,
                        &surface,
                        origin,
                        scale,
                        transform.alpha,
                        Kind::Unspecified,
                    )
                    .into_iter()
                    .map(|element| {
                        RescaleRenderElement::from_element(
                            element,
                            (0, 0).into(),
                            (transform.scale_x, transform.scale_y),
                        )
                    })
                    .map(|element| {
                        RelocateRenderElement::from_element(element, offset, Relocate::Relative)
                    })
                    .map(HuginnElement::Workspace),
                );
            }
            SceneItem::WorkspaceCard(buffer, transform) => {
                let offset = Point::<f64, Logical>::from((transform.offset_x, transform.offset_y))
                    .to_physical_precise_round::<f64, i32>(scale);
                let element = SolidColorRenderElement::from_buffer(
                    buffer,
                    (0, 0),
                    scale,
                    transform.alpha,
                    Kind::Unspecified,
                );
                let element = RescaleRenderElement::from_element(
                    element,
                    (0, 0).into(),
                    (transform.scale_x, transform.scale_y),
                );
                out.push(HuginnElement::WorkspaceCard(
                    RelocateRenderElement::from_element(element, offset, Relocate::Relative),
                ));
            }
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
            SceneItem::Ring(buffer, rect, alpha) => {
                out.push(HuginnElement::Ring(SolidColorRenderElement::from_buffer(
                    buffer,
                    Point::<i32, Logical>::from((rect.x(), rect.y()))
                        .to_physical_precise_round::<f64, i32>(scale),
                    scale,
                    alpha,
                    Kind::Unspecified,
                )));
            }
        }
    }

    out
}
