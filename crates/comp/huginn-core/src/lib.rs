//! Window-management model for the Huginn compositor.
//!
//! This crate is the design surface of the compositor: what a window is, which
//! workspace it lives on, where focus goes when one closes, and how tiles are
//! laid out. It knows nothing about Wayland, DRM, or the GPU, which is exactly
//! the point — it compiles and tests on any host, so window-management
//! behaviour can be iterated on without a display attached.
//!
//! `huginn-comp` owns the other half: it maps [`WindowId`] to a real
//! `WlSurface`, drives the event loop, and turns the output of [`Space::arrange`]
//! into `xdg_toplevel::configure` events.
//!
//! ```
//! use huginn_core::{Space, geometry::Rect};
//!
//! let mut space = Space::new(Rect::from_xywh(0, 0, 1920, 1080));
//! let editor = space.open_window();
//! let terminal = space.open_window();
//!
//! // Two tiled windows share the screen; both get fresh geometry.
//! assert_eq!(space.arrange().len(), 2);
//! assert_eq!(space.focused(), Some(terminal));
//!
//! // Closing the focused window hands focus back rather than dropping it.
//! space.close_window(terminal);
//! assert_eq!(space.focused(), Some(editor));
//! ```

pub mod geometry;
pub mod layout;
pub mod window;
pub mod workspace;

use std::collections::BTreeMap;

use geometry::Rect;
use window::{Window, WindowId, WindowMode};
use workspace::{Workspace, WorkspaceId};

/// How many workspaces exist at startup.
///
/// Fixed rather than dynamic for now: static workspaces let a panel render a
/// stable row of indicators, which is the behaviour `muninn` is built around.
const DEFAULT_WORKSPACES: u64 = 9;

/// The complete window-management state of one seat.
#[derive(Debug)]
pub struct Space {
    windows: BTreeMap<WindowId, Window>,
    workspaces: Vec<Workspace>,
    active: usize,
    /// Usable area after layer-shell exclusive zones (panels, docks) are
    /// subtracted. Single-output for now; multi-output arrives with the udev
    /// backend, at which point this becomes a per-output field.
    area: Rect,
    next_window: u64,
}

impl Space {
    /// Create a space with [`DEFAULT_WORKSPACES`] empty workspaces.
    pub fn new(area: Rect) -> Self {
        Self {
            windows: BTreeMap::new(),
            workspaces: (1..=DEFAULT_WORKSPACES)
                .map(|n| Workspace::new(WorkspaceId::from_raw(n)))
                .collect(),
            active: 0,
            area,
            next_window: 1,
        }
    }

    /// The area available to windows.
    pub const fn area(&self) -> Rect {
        self.area
    }

    /// Update the usable area, e.g. when a panel claims an exclusive zone or an
    /// output is resized. Call [`Self::arrange`] afterwards to apply it.
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// Register a new window on the active workspace and focus it.
    pub fn open_window(&mut self) -> WindowId {
        let id = WindowId::from_raw(self.next_window);
        self.next_window += 1;
        self.windows.insert(id, Window::new(id));
        self.workspaces[self.active].insert(id);
        id
    }

    /// Forget a window, removing it from whichever workspace holds it.
    ///
    /// Returns whether the window existed. Safe to call for a window on an
    /// inactive workspace, which is the common case for a client that exits on
    /// its own.
    pub fn close_window(&mut self, id: WindowId) -> bool {
        if self.windows.remove(&id).is_none() {
            return false;
        }
        for ws in &mut self.workspaces {
            if ws.remove(id) {
                break;
            }
        }
        true
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active]
    }

    /// Index of the active workspace, zero-based.
    pub const fn active_index(&self) -> usize {
        self.active
    }

    /// The focused window on the active workspace, if any.
    pub fn focused(&self) -> Option<WindowId> {
        self.active_workspace().focused()
    }

    /// Switch to workspace `index`. Out-of-range indices are ignored rather
    /// than clamped, so a stray keybinding cannot silently jump to workspace 9.
    pub fn activate_workspace(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// Send the focused window to workspace `index`, keeping the current
    /// workspace active.
    pub fn send_focused_to_workspace(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() || index == self.active {
            return false;
        }
        let Some(id) = self.focused() else {
            return false;
        };
        self.workspaces[self.active].remove(id);
        self.workspaces[index].insert(id);
        true
    }

    /// Recompute geometry for every window on the active workspace.
    ///
    /// Returns only the windows whose geometry actually changed. The compositor
    /// sends one `configure` per entry, so returning the full set instead would
    /// mean a configure storm on every unrelated state change — clients treat
    /// that as a resize and re-render.
    pub fn arrange(&mut self) -> Vec<(WindowId, Rect)> {
        let area = self.area;
        let ws = &self.workspaces[self.active];

        let tiled: Vec<WindowId> = ws
            .windows()
            .iter()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(Window::is_tiled))
            .collect();

        let rects = ws.layout().arrange(area, tiled.len());
        debug_assert_eq!(rects.len(), tiled.len(), "layout returned the wrong count");

        let mut changed = Vec::new();
        for (id, rect) in tiled.iter().zip(rects) {
            let Some(win) = self.windows.get_mut(id) else {
                continue;
            };
            let sized = Rect::new(rect.origin, win.hints.clamp(rect.size));
            if win.geometry != sized {
                win.geometry = sized;
                changed.push((*id, sized));
            }
        }

        // Floating and fullscreen windows sit outside the layout, but still
        // have to follow the area when an output resizes or a panel appears.
        for id in ws.windows() {
            let Some(win) = self.windows.get_mut(id) else {
                continue;
            };
            let wanted = match win.mode {
                WindowMode::Tiled => continue,
                WindowMode::Fullscreen => area,
                WindowMode::Floating => win.geometry.constrain_to(area),
            };
            if win.geometry != wanted {
                win.geometry = wanted;
                changed.push((*id, wanted));
            }
        }

        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::window::SizeHints;
    use geometry::Size;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    fn space() -> Space {
        Space::new(SCREEN)
    }

    #[test]
    fn arrange_is_idempotent() {
        let mut s = space();
        s.open_window();
        s.open_window();
        assert_eq!(s.arrange().len(), 2, "first arrange assigns geometry");
        assert!(
            s.arrange().is_empty(),
            "a second arrange with no state change must configure nothing"
        );
    }

    #[test]
    fn tiled_windows_stay_inside_the_area() {
        let mut s = space();
        for _ in 0..5 {
            s.open_window();
        }
        s.arrange();
        for id in s.active_workspace().windows() {
            let g = s.window(*id).expect("window is registered").geometry;
            assert!(g.x() >= SCREEN.x() && g.right() <= SCREEN.right(), "{g:?} escaped horizontally");
            assert!(g.y() >= SCREEN.y() && g.bottom() <= SCREEN.bottom(), "{g:?} escaped vertically");
        }
    }

    #[test]
    fn shrinking_the_area_pulls_floating_windows_back_on_screen() {
        let mut s = space();
        let w = s.open_window();
        let win = s.window_mut(w).expect("just opened");
        win.mode = WindowMode::Floating;
        win.geometry = Rect::from_xywh(1600, 900, 400, 300);
        s.arrange();

        // Unplugging the big monitor.
        s.set_area(Rect::from_xywh(0, 0, 800, 600));
        s.arrange();

        let g = s.window(w).expect("still open").geometry;
        assert!(g.right() <= 800 && g.bottom() <= 600, "{g:?} left the screen");
    }

    #[test]
    fn a_panel_claiming_space_reflows_tiled_windows() {
        let mut s = space();
        s.open_window();
        s.arrange();

        // A 40px top panel takes an exclusive zone.
        s.set_area(Rect::new(Point::new(0, 40), Size::new(1920, 1040)));
        let changed = s.arrange();
        assert_eq!(changed.len(), 1);
        assert!(changed[0].1.y() >= 40, "window overlaps the panel");
    }

    #[test]
    fn size_hints_are_honoured_over_layout_geometry() {
        let mut s = space();
        let w = s.open_window();
        s.window_mut(w).expect("just opened").hints = SizeHints {
            min: None,
            max: Some(Size::new(640, 480)),
        };
        s.arrange();
        assert_eq!(s.window(w).expect("still open").geometry.size, Size::new(640, 480));
    }

    #[test]
    fn fullscreen_covers_the_whole_area_and_restores() {
        let mut s = space();
        let a = s.open_window();
        s.open_window();
        s.arrange();
        let tiled_geom = s.window(a).expect("open").geometry;

        s.window_mut(a).expect("open").fullscreen(SCREEN);
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, SCREEN);

        s.window_mut(a).expect("open").unfullscreen(WindowMode::Tiled);
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, tiled_geom);
    }

    #[test]
    fn windows_on_inactive_workspaces_are_not_arranged() {
        let mut s = space();
        let a = s.open_window();
        s.arrange();
        let before = s.window(a).expect("open").geometry;

        s.activate_workspace(1);
        s.open_window();
        s.set_area(Rect::from_xywh(0, 0, 640, 480));
        s.arrange();

        assert_eq!(
            s.window(a).expect("open").geometry,
            before,
            "a hidden window must not be reconfigured"
        );
    }

    #[test]
    fn closing_a_window_on_an_inactive_workspace_works() {
        let mut s = space();
        let a = s.open_window();
        s.activate_workspace(3);
        assert!(s.close_window(a));
        assert!(s.workspaces().iter().all(|w| w.is_empty()));
    }

    #[test]
    fn sending_a_window_away_moves_focus_to_a_neighbour() {
        let mut s = space();
        let a = s.open_window();
        let b = s.open_window();
        assert_eq!(s.focused(), Some(b));

        assert!(s.send_focused_to_workspace(2));
        assert_eq!(s.focused(), Some(a), "focus stays on this workspace");

        s.activate_workspace(2);
        assert_eq!(s.focused(), Some(b), "and the window arrived focused");
    }

    #[test]
    fn out_of_range_workspace_switches_are_rejected() {
        let mut s = space();
        assert!(!s.activate_workspace(99));
        assert_eq!(s.active_index(), 0);
    }

    #[test]
    fn closing_an_unknown_window_is_a_no_op() {
        let mut s = space();
        assert!(!s.close_window(WindowId::from_raw(999)));
    }
}
