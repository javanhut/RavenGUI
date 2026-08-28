//! The compositor's view of a toplevel window.
//!
//! Deliberately holds no Wayland handle. `huginn-comp` keeps the mapping from
//! [`WindowId`] to the real `WlSurface`, which is what allows every layout and
//! focus decision to be exercised in a unit test with no display attached.

use crate::geometry::{Rect, Size};

/// Opaque handle to a window. Allocated by [`crate::Space`]; never reused, so a
/// stale id is always detectably stale rather than silently aliasing a new
/// window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl WindowId {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// How a window participates in layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowMode {
    /// Placed by the workspace's tile tree. See [`crate::tiles`].
    #[default]
    Tiled,
    /// Keeps its own geometry; floats above tiled windows.
    Floating,
    /// Covers its output entirely, above everything except layer-shell overlays.
    Fullscreen,
    /// Kept alive but absent from layout and rendering until explicitly restored.
    Minimized,
}

/// Size hints a client advertised. A client may ask; the layout decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeHints {
    pub min: Option<Size>,
    pub max: Option<Size>,
}

impl SizeHints {
    /// Clamp `size` into the hinted range. `min` wins over `max` when a client
    /// advertises a contradictory pair, which does happen in the wild.
    pub fn clamp(self, size: Size) -> Size {
        let mut out = size;
        if let Some(max) = self.max {
            out = Size::new(out.w.min(max.w), out.h.min(max.h));
        }
        if let Some(min) = self.min {
            out = Size::new(out.w.max(min.w), out.h.max(min.h));
        }
        out
    }
}

/// A managed toplevel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    id: WindowId,
    /// Geometry as the compositor last assigned it. For a tiled window this is
    /// overwritten on every arrange; for a floating one it is authoritative.
    pub geometry: Rect,
    /// Geometry and mode to restore when leaving fullscreen.
    restore: Option<(Rect, WindowMode)>,
    pub mode: WindowMode,
    pub hints: SizeHints,
    pub app_id: Option<String>,
    pub title: Option<String>,
}

impl Window {
    pub(crate) fn new(id: WindowId) -> Self {
        Self {
            id,
            geometry: Rect::ZERO,
            restore: None,
            mode: WindowMode::default(),
            hints: SizeHints::default(),
            app_id: None,
            title: None,
        }
    }

    pub const fn id(&self) -> WindowId {
        self.id
    }

    /// True if this window's geometry comes from the layout engine.
    pub const fn is_tiled(&self) -> bool {
        matches!(self.mode, WindowMode::Tiled)
    }

    /// Whether this window keeps its own geometry instead of a tile.
    ///
    /// The distinction that matters to the tile tree is floating versus not,
    /// which is *not* the same as tiled versus not: a fullscreen window is not
    /// tiled, but it still owns the tile it will return to. Evicting it would
    /// mean un-fullscreening dropped it into whatever slot happened to be free,
    /// and the spec is explicit that fullscreen is a state inside the pane
    /// rather than a departure from it.
    pub const fn is_floating(&self) -> bool {
        matches!(self.mode, WindowMode::Floating)
    }

    /// Whether this window currently participates in the workspace layout.
    pub const fn is_minimized(&self) -> bool {
        matches!(self.mode, WindowMode::Minimized)
    }

    /// Put the window in the background without closing its client.
    pub fn minimize(&mut self) {
        self.mode = WindowMode::Minimized;
    }

    /// Bring a minimized window back into the layout as an ordinary tile.
    ///
    /// Restoring is not fullscreen: the window takes its place in the
    /// workspace at whatever size the layout gives it, so the panels and the
    /// client's own chrome (a browser's tab strip, say) stay usable. Returns
    /// whether the window was minimized.
    pub fn unminimize(&mut self) -> bool {
        if self.mode != WindowMode::Minimized {
            return false;
        }
        self.mode = WindowMode::Tiled;
        true
    }

    /// Enter fullscreen over `output`, remembering where to return to.
    ///
    /// `output` is the whole output, not the area left after panels: a
    /// fullscreen window covers the screen edge to edge.
    pub fn fullscreen(&mut self, output: Rect) {
        if self.mode != WindowMode::Fullscreen {
            self.restore = Some((self.geometry, self.mode));
        }
        self.mode = WindowMode::Fullscreen;
        self.geometry = output;
    }

    /// Leave fullscreen, going back to the mode and geometry it came from.
    ///
    /// A window that was tiled before going fullscreen returns to `Tiled` and
    /// the next arrange overwrites its geometry anyway; restoring it here keeps
    /// the frame before that arrange from flashing at fullscreen size. One
    /// that was floating gets its old rect back for good. A window that was
    /// never anything else — fullscreen from its first frame — is tiled.
    pub fn unfullscreen(&mut self) {
        if self.mode != WindowMode::Fullscreen {
            return;
        }
        match self.restore.take() {
            Some((geometry, mode)) => {
                self.geometry = geometry;
                self.mode = mode;
            }
            None => self.mode = WindowMode::Tiled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    fn win() -> Window {
        Window::new(WindowId::from_raw(1))
    }

    #[test]
    fn fullscreen_round_trip_restores_geometry() {
        let mut w = win();
        w.mode = WindowMode::Floating;
        w.geometry = Rect::from_xywh(10, 10, 300, 200);

        w.fullscreen(Rect::from_xywh(0, 0, 1920, 1080));
        assert_eq!(w.geometry, Rect::from_xywh(0, 0, 1920, 1080));

        w.unfullscreen();
        assert_eq!(w.geometry, Rect::from_xywh(10, 10, 300, 200));
        assert_eq!(w.mode, WindowMode::Floating);
    }

    #[test]
    fn double_fullscreen_does_not_clobber_restore_geometry() {
        let mut w = win();
        w.geometry = Rect::from_xywh(10, 10, 300, 200);
        w.fullscreen(Rect::from_xywh(0, 0, 1920, 1080));
        // A client re-requesting fullscreen must not make the restore point
        // the fullscreen rect itself.
        w.fullscreen(Rect::from_xywh(0, 0, 2560, 1440));
        w.unfullscreen();
        assert_eq!(w.geometry, Rect::from_xywh(10, 10, 300, 200));
        assert_eq!(w.mode, WindowMode::Tiled);
    }

    #[test]
    fn min_beats_max_when_hints_contradict() {
        let hints = SizeHints {
            min: Some(Size::new(400, 300)),
            max: Some(Size::new(200, 100)),
        };
        assert_eq!(hints.clamp(Size::new(800, 600)), Size::new(400, 300));
    }
}
