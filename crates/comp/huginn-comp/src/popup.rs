//! Popups: menus, dropdowns, tooltips, and the grabs that dismiss them.
//!
//! An `xdg_popup` is not a window. The core never sees one, it is never tiled,
//! and it has no place in a workspace — it is a rectangle a client hangs off a
//! surface it already owns, positioned by rules the client sends and the
//! compositor is required to apply. Keeping all of that here means `state.rs`
//! stays the seam between Wayland and `huginn-core` rather than growing a
//! second layout engine that the core knows nothing about.
//!
//! Three things have to be right for a menu to work:
//!
//! * **Placement.** The client sends an anchor rectangle and a gravity, both
//!   relative to the parent's *window geometry* rather than its surface. A
//!   client with a drop shadow has those two origins some way apart, so a popup
//!   placed against the surface origin hangs off the wrong corner.
//! * **Constraining.** The positioner is a request, not an instruction. A menu
//!   anchored to a button near the bottom of the screen would open past the
//!   edge, and the protocol says the compositor flips or slides it back on.
//!   Handing the client's geometry straight through is what puts the bottom
//!   half of a long menu underneath the monitor.
//! * **The grab.** A menu is modal: the next click either picks an item or
//!   dismisses it, and either way it does not reach whatever is underneath.
//!   That is `PopupGrab`, and without it a menu stays on screen after it has
//!   been clicked away and the click lands on the window behind it.

use smithay::{
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy,
        find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::{Seat, pointer::Focus},
    reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle, Serial},
    wayland::{
        compositor::{get_parent, with_states},
        shell::xdg::{PopupSurface, SurfaceCachedState},
    },
};

use huginn_core::geometry::Rect;

use crate::state::{Huginn, SceneItem};

/// The window geometry a surface set with `xdg_surface::set_window_geometry`.
///
/// Zero when it set none, which is the common case and also the correct
/// fallback: a client that never called it is saying its surface *is* its
/// window.
pub(crate) fn window_geometry(surface: &WlSurface) -> Rectangle<i32, Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
    })
    .unwrap_or_default()
}

impl Huginn {
    /// Every popup hanging off `root`, front to back, ready to paint.
    ///
    /// `rect` is where `root`'s *surface* was placed. The popup tree's offsets
    /// are measured from `root`'s *window geometry*, so that origin has to be
    /// added back before anything lands in global coordinates — and subtracted
    /// again on the popup's own side, since its geometry origin is where the
    /// positioner aimed but its surface origin is what gets drawn.
    ///
    /// `popups_for_surface` yields children before their parents, which is
    /// exactly the order a submenu wants: on top of the menu that opened it.
    pub(crate) fn popups_of(&self, root: &WlSurface, rect: Rect) -> Vec<SceneItem<'_>> {
        let root_geometry = window_geometry(root);
        let origin: Point<i32, Logical> = (rect.x(), rect.y()).into();

        smithay::desktop::PopupManager::popups_for_surface(root)
            .map(|(popup, offset)| {
                let geometry = popup.geometry();
                let location = origin + root_geometry.loc + offset - geometry.loc;
                SceneItem::Surface(
                    popup.wl_surface().clone(),
                    Rect::from_xywh(location.x, location.y, geometry.size.w, geometry.size.h),
                )
            })
            .collect()
    }

    /// Where the surface a popup ultimately hangs off was placed, if it is
    /// still on screen.
    ///
    /// A popup's root is a toplevel or a layer surface — a panel's menu is as
    /// legitimate as a window's. Both are looked up the same way they are
    /// painted, so a popup cannot be placed against geometry the renderer
    /// disagrees with.
    fn root_rect(&self, root: &WlSurface) -> Option<Rect> {
        self.render_list()
            .into_iter()
            .find(|(surface, _)| surface == root)
            .map(|(_, rect)| rect)
            .or_else(|| {
                self.all_layers()
                    .into_iter()
                    .find(|(layer, _)| layer.wl_surface() == root)
                    .map(|(_, rect)| rect)
            })
    }

    /// Apply the protocol's flip/slide/resize rules so the popup fits on the
    /// output.
    ///
    /// The target rectangle has to be expressed in the same frame the client's
    /// positioner is: relative to the window geometry of the popup's immediate
    /// parent. Walking the output rectangle back through the parent chain is
    /// what `get_popup_toplevel_coords` is for — a submenu is constrained
    /// against the screen, not against the menu that opened it.
    ///
    /// A popup whose root has gone is left alone. There is nothing sensible to
    /// constrain it against, and the client is about to be told the popup is
    /// done anyway.
    pub(crate) fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Some(rect) = self.root_rect(&root) else {
            return;
        };

        let output = self.output_area();
        let mut target = Rectangle::new(
            (output.x(), output.y()).into(),
            (output.w(), output.h()).into(),
        );
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= Point::from((rect.x(), rect.y())) + window_geometry(&root).loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// Hand a popup the modal grab it asked for.
    ///
    /// Refusing is a normal outcome, not an error: the protocol only allows a
    /// grab from the topmost popup of a chain and only against a serial the
    /// client actually received. `grab_popup` enforces both and posts the
    /// protocol error itself, so a rejection here just means no menu opens.
    pub(crate) fn take_popup_grab(
        &mut self,
        surface: PopupSurface,
        seat: &wl_seat::WlSeat,
        serial: Serial,
    ) {
        let Some(seat) = Seat::<Self>::from_resource(seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };

        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };

        // A grab already in progress keeps the seat unless this request is the
        // one that started it, or the one that started the popup below it in
        // the chain. Stealing the seat from an unrelated grab would leave that
        // one believing it still has input.
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            // `current_grab` is an Option, so this can clear focus too.
            self.set_keyboard_focus(grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }

        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or(grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            // Focus::Keep: the pointer is already over the surface that opened
            // the menu, and clearing focus here would send it a leave for a
            // click it is in the middle of handling.
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    /// Whether `surface` belongs to a popup rather than to something the core
    /// knows about.
    ///
    /// Click-to-focus asks this: a click on a menu belongs to the window that
    /// opened it, and moving focus to whichever window happens to lie under the
    /// menu would dismiss the menu and focus the wrong thing in one gesture.
    ///
    /// Hit testing returns the deepest subsurface it found, so the answer has
    /// to be looked up against the top of the subsurface tree. A menu that
    /// draws its items into subsurfaces would otherwise not be recognised as a
    /// menu at all — and that is exactly the menu whose items are worth
    /// clicking.
    pub(crate) fn is_popup(&self, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.popups.find_popup(&root).is_some()
    }
}
