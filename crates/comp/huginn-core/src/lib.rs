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
pub mod layer;
pub mod layout;
pub mod scale;
pub mod strip;
pub mod tiles;
pub mod window;
pub mod workspace;

use std::collections::BTreeMap;

use geometry::{Dir, Rect};
use window::{Window, WindowId, WindowMode};
use workspace::{Direction, Layout, Workspace, WorkspaceId};

/// How many workspaces exist at startup.
///
/// Fixed rather than dynamic for now: static workspaces let a panel render a
/// stable row of indicators, which is the behaviour `muninn` is built around.
const DEFAULT_WORKSPACES: u64 = 9;

/// Gutter used until the compositor supplies its own. See [`Space::set_gap`].
const DEFAULT_GAP: i32 = 8;

/// One screen the desktop spans, in logical coordinates shared by every
/// screen.
///
/// `output` is the whole of it, which is what fullscreen covers. `area` is
/// what is left for windows once the panels on that screen have taken their
/// exclusive zones. Each screen has its own pair: a dock on the laptop panel
/// reserves nothing on the monitor beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputArea {
    pub output: Rect,
    pub area: Rect,
}

impl OutputArea {
    /// A screen with no panels on it yet.
    pub const fn new(output: Rect) -> Self {
        Self {
            output,
            area: output,
        }
    }
}

/// The complete window-management state of one seat.
#[derive(Debug)]
pub struct Space {
    windows: BTreeMap<WindowId, Window>,
    workspaces: Vec<Workspace>,
    /// The focused workspace. Its output is the focused output: the one
    /// keybindings act on, new windows open on, and the shell's panels sit on.
    active: usize,
    /// Every screen, in one global coordinate space. Never empty: with no
    /// monitor connected the last known geometry is kept, so the desktop has
    /// somewhere to be when one comes back.
    outputs: Vec<OutputArea>,
    /// Which workspace each output is showing, parallel to `outputs`.
    ///
    /// The invariant that makes multi-monitor mean anything: every output shows
    /// exactly one workspace, no workspace is visible on two outputs, and the
    /// active workspace is the one visible on the focused output.
    visible: Vec<usize>,
    /// Space between tiled windows and around the edge of the pane.
    ///
    /// Held here rather than read from a constant so `huginn-core` keeps
    /// knowing nothing about the desktop's appearance; the compositor sets it
    /// once from its theme.
    gap: i32,
    /// How many panes the carousel shows at once.
    ///
    /// Held beside [`Self::gap`] and for the same reason: how wide a pane should
    /// be is an appearance decision with geometric consequences, and this crate
    /// does not get to make appearance decisions. The compositor sets it once
    /// from its theme.
    carousel_columns: u32,
    /// Where the carousel is scrolled to right now, when that is not simply
    /// wherever focus is.
    ///
    /// `None` means "wherever focus is" — the layout resolves the offset itself
    /// and the strip is wherever it belongs. `Some` is the compositor sliding
    /// the strip towards that place over several frames, which is the one thing
    /// this crate cannot work out for itself because it has no clock.
    carousel_offset: Option<i32>,
    /// The workspace whose strip a touchpad swipe currently has hold of.
    ///
    /// While this names the active workspace the fingers decide where the strip
    /// sits and [`Self::update_carousel_target`] stops deriving it from focus.
    /// Without that, every frame of a drag would pull the strip back to
    /// whichever pane happened to be focused when the fingers went down, and
    /// the gesture would spend its whole length fighting the layout.
    ///
    /// A workspace id rather than a flag, so switching workspace mid-swipe ends
    /// the drag for the one you left instead of freezing the one you arrive on.
    carousel_drag: Option<WorkspaceId>,
    next_window: u64,
}

impl Space {
    /// Create a space with [`DEFAULT_WORKSPACES`] empty workspaces on one
    /// output covering `area`.
    pub fn new(area: Rect) -> Self {
        Self {
            windows: BTreeMap::new(),
            workspaces: (1..=DEFAULT_WORKSPACES)
                .map(|n| Workspace::new(WorkspaceId::from_raw(n)))
                .collect(),
            active: 0,
            outputs: vec![OutputArea::new(area)],
            visible: vec![0],
            gap: DEFAULT_GAP,
            carousel_columns: strip::DEFAULT_COLUMNS,
            carousel_offset: None,
            carousel_drag: None,
            next_window: 1,
        }
    }

    /// Set the gutter between tiled windows. Call [`Self::arrange`] after.
    pub fn set_gap(&mut self, gap: i32) {
        self.gap = gap.max(0);
    }

    /// Set how many panes the carousel shows at once. Call [`Self::arrange`]
    /// after. Clamped to at least one, since zero columns has no layout.
    pub fn set_carousel_columns(&mut self, columns: u32) {
        self.carousel_columns = columns.max(1);
    }

    /// Flip the active workspace between tiling and the carousel, and report
    /// the layout now in force. Call [`Self::arrange`] after.
    pub fn toggle_layout(&mut self) -> Layout {
        self.workspaces[self.active].toggle_layout()
    }

    /// The tiled windows of the active workspace, in order.
    fn tiled_windows(&self) -> Vec<WindowId> {
        self.workspaces[self.active]
            .windows()
            .iter()
            .copied()
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| !w.is_floating() && !w.is_minimized())
            })
            .collect()
    }

    /// Work out where the active workspace's strip should now be settled,
    /// record it, and return it. `None` when that workspace is not a carousel.
    ///
    /// The compositor animates towards this. Nothing here moves on its own.
    ///
    /// This commits rather than merely reporting, because the answer depends on
    /// where the strip already is: a pane that is on screen leaves the offset
    /// alone, so the previous position has to be the one that is read next time.
    /// Reporting without committing would compute every step from the same
    /// stale origin and undo the nudge.
    pub fn update_carousel_target(&mut self) -> Option<i32> {
        let tiled = self.tiled_windows();
        let (area, gap, columns) = (self.area(), self.gap, self.carousel_columns);
        // Read before the workspace is borrowed, not because the order matters
        // to the logic but because it does to the borrow checker.
        let drag = self.carousel_drag;
        let ws = &mut self.workspaces[self.active];
        if ws.layout() != Layout::Carousel {
            return None;
        }
        if drag == Some(ws.id()) {
            // The fingers are deciding. Report where they have put it rather
            // than where focus would ask for — a drag is the one time the strip
            // is allowed to leave the focused pane off screen, because the pane
            // it settles on is not chosen until they lift.
            return Some(ws.scroll());
        }
        let settled = strip::target_offset(&tiled, ws.focused(), area, gap, columns, ws.scroll());
        ws.set_scroll(settled);
        Some(settled)
    }

    /// Hold the carousel at `offset` while it is sliding, or hand it back to
    /// focus with `None`. Call [`Self::arrange`] after.
    pub fn set_carousel_offset(&mut self, offset: Option<i32>) {
        self.carousel_offset = offset;
    }

    /// Take hold of the active workspace's strip for a touchpad swipe, and
    /// report the offset it is resting at — where the drag starts from.
    ///
    /// A tiled workspace becomes a carousel here, because the gesture is how
    /// you get there. Three fingers on the strip is the same statement
    /// [`Self::toggle_layout`] makes, so it leaves the workspace in the same
    /// place and the layout stays put when the fingers lift.
    ///
    /// `None` when the strip has nowhere to go: no panes at all, or few enough
    /// that they already fill the viewport. Both are the same thing to a swipe.
    /// There is nothing to take hold of, and flipping the layout under a
    /// gesture that can do nothing visible would be a mode change the user
    /// never sees happen — the workspace would sit in [`Layout::Carousel`] with
    /// nothing on screen to say so, and the next `Super`+`Ctrl`+`C` would read
    /// as inverted because it toggles back to tiling.
    ///
    /// "Nowhere to go" is [`strip::max_offset`] rather than an emptiness test,
    /// because at the default two columns a workspace of one or two panes lays
    /// out identically under both layouts. Refusing on emptiness alone accepted
    /// exactly those cases, which is the invisible flip this rule exists to
    /// prevent.
    pub fn begin_carousel_drag(&mut self) -> Option<i32> {
        // Subsumes the empty case: `max_offset` is zero for a strip with no
        // panes, so there is no separate emptiness test to keep in step.
        let tiled = self.tiled_windows();
        if strip::max_offset(&tiled, self.area(), self.gap, self.carousel_columns) == 0 {
            return None;
        }
        let ws = &mut self.workspaces[self.active];
        ws.set_layout(Layout::Carousel);
        let (id, scroll) = (ws.id(), ws.scroll());
        self.carousel_drag = Some(id);
        Some(scroll)
    }

    /// Whether a swipe has hold of the active workspace's strip.
    ///
    /// The compositor asks so it can put the strip exactly where the fingers
    /// are rather than sliding towards them. A drag that animated would sit a
    /// fixed distance behind the hand moving it, which is the one thing direct
    /// manipulation must not do.
    pub fn carousel_dragging(&self) -> bool {
        self.carousel_drag == Some(self.workspaces[self.active].id())
    }

    /// Move the held strip to `offset`, clamped to its own ends, and report
    /// where it actually landed. Call [`Self::arrange`] after.
    ///
    /// The clamp is the whole of the edge behaviour: the strip stops and the
    /// fingers keep going. No rubber band, because there is nothing past the
    /// end to hint at — the strip is the entire workspace, and stretching it
    /// would suggest content that does not exist.
    ///
    /// `None`, and nothing moves, unless [`Self::begin_carousel_drag`] took
    /// hold of *this* workspace first.
    pub fn drag_carousel(&mut self, offset: i32) -> Option<i32> {
        let held = self.carousel_drag?;
        let tiled = self.tiled_windows();
        let (area, gap, columns) = (self.area(), self.gap, self.carousel_columns);
        let ws = &mut self.workspaces[self.active];
        if held != ws.id() {
            return None;
        }
        let landed = offset.clamp(0, strip::max_offset(&tiled, area, gap, columns));
        ws.set_scroll(landed);
        Some(landed)
    }

    /// Let go of the strip: settle it onto the pane nearest where the fingers
    /// left it, focus that pane, and report it. Call [`Self::arrange`] after.
    ///
    /// Focusing is what makes the settle stick. Everything else about the
    /// carousel derives the scroll from focus, so a drag that moved the strip
    /// without moving focus would be undone by the very next arrange — and the
    /// pane you swiped to would not be the one the keyboard was talking to,
    /// which is a worse bug than the strip springing back, because it is quiet.
    ///
    /// Ends the drag either way, so a swipe over an emptied workspace releases
    /// its hold rather than leaving the strip pinned.
    pub fn end_carousel_drag(&mut self) -> Option<WindowId> {
        let held = self.carousel_drag.take()?;
        let tiled = self.tiled_windows();
        let (area, gap, columns) = (self.area(), self.gap, self.carousel_columns);
        let ws = &mut self.workspaces[self.active];
        if held != ws.id() {
            return None;
        }
        let (pane, offset) = strip::snap(&tiled, area, gap, columns, ws.scroll())?;
        ws.set_scroll(offset);
        ws.focus(pane);
        Some(pane)
    }

    /// The area available to windows on the focused output.
    pub fn area(&self) -> Rect {
        self.outputs[self.focused_output()].area
    }

    /// Update the usable area of the focused output, e.g. when a panel claims
    /// an exclusive zone. Call [`Self::arrange`] afterwards to apply it.
    pub fn set_area(&mut self, area: Rect) {
        let index = self.focused_output();
        self.outputs[index].area = area;
    }

    /// The whole rectangle of the focused output, which is what fullscreen
    /// covers there.
    pub fn output(&self) -> Rect {
        self.outputs[self.focused_output()].output
    }

    /// Update the whole rectangle of the focused output. Call
    /// [`Self::arrange`] afterwards.
    pub fn set_output(&mut self, output: Rect) {
        let index = self.focused_output();
        self.outputs[index].output = output;
    }

    /// Every screen the desktop spans. Never empty.
    pub fn outputs(&self) -> &[OutputArea] {
        &self.outputs
    }

    /// The output the active workspace is on: where focus is.
    pub fn focused_output(&self) -> usize {
        self.workspaces[self.active].output()
    }

    /// The workspace each output is showing, as `(output, workspace)` index
    /// pairs in output order.
    pub fn visible_workspaces(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.visible.iter().copied().enumerate()
    }

    /// The workspace output `index` is showing.
    pub fn visible_on(&self, index: usize) -> Option<usize> {
        self.visible.get(index).copied()
    }

    /// The output whose rectangle contains `point`, if any.
    pub fn output_at(&self, point: geometry::Point) -> Option<usize> {
        self.outputs
            .iter()
            .position(|screen| screen.output.contains(point))
    }

    /// Update the usable area of one output. Call [`Self::arrange`] after.
    pub fn set_output_area(&mut self, index: usize, area: Rect) -> bool {
        match self.outputs.get_mut(index) {
            Some(screen) if screen.area != area => {
                screen.area = area;
                true
            }
            _ => false,
        }
    }

    /// Replace the set of screens. Call [`Self::arrange`] afterwards.
    ///
    /// Reconciles the workspaces with the new set rather than resetting them:
    /// a monitor unplugged sends its workspaces to the screen that remains,
    /// windows and focus intact, and a monitor plugged in is given a workspace
    /// of its own -- the first one nobody is looking at -- so it comes up as a
    /// desktop rather than a blank. An empty list keeps the outputs there
    /// were; the desktop needs somewhere to be while nothing is connected.
    pub fn set_outputs(&mut self, outputs: Vec<OutputArea>) {
        if outputs.is_empty() {
            return;
        }
        // Each previous output's areas are replaced in order; a caller that
        // rebuilds the list from scratch on every hotplug gets stable
        // workspace placement for the screens that stayed, which is what
        // keeps the laptop panel's desktop on the laptop panel.
        let count = outputs.len();
        self.outputs = outputs;

        // Workspaces on a screen that is gone move to the last one left.
        let last = count - 1;
        for workspace in &mut self.workspaces {
            if workspace.output() >= count {
                workspace.set_output(last);
            }
        }
        self.visible.truncate(count);
        // A screen that survived may now show a workspace that also moved
        // here from a lost screen; the first claim wins and the rest are
        // hidden, which is what a workspace on a screen you cannot see is.
        let mut seen = Vec::with_capacity(count);
        for index in 0..self.visible.len() {
            let workspace = self.visible[index];
            if seen.contains(&workspace) || self.workspaces[workspace].output() != index {
                self.visible[index] = usize::MAX;
            } else {
                seen.push(workspace);
            }
        }
        // New screens, and survivors left without a workspace, take the first
        // workspace not visible anywhere. The active workspace is never
        // handed away: it is what the person is working in.
        for index in 0..count {
            if index < self.visible.len() && self.visible[index] != usize::MAX {
                continue;
            }
            let pick = (0..self.workspaces.len())
                .find(|candidate| {
                    !seen.contains(candidate)
                        && *candidate != self.active
                        && self.workspaces[*candidate].output() == index
                })
                .or_else(|| {
                    (0..self.workspaces.len())
                        .find(|candidate| !seen.contains(candidate) && *candidate != self.active)
                })
                // Nine workspaces and more than nine screens: share the last.
                .unwrap_or(self.active);
            self.workspaces[pick].set_output(index);
            seen.push(pick);
            if index < self.visible.len() {
                self.visible[index] = pick;
            } else {
                self.visible.push(pick);
            }
        }
        // The active workspace must be visible on its own output.
        let focused = self.workspaces[self.active].output();
        if self.visible[focused] != self.active {
            self.visible[focused] = self.active;
        }
        debug_assert_eq!(self.visible.len(), self.outputs.len());
    }

    /// Move focus to output `index`, onto the workspace it is showing.
    pub fn focus_output(&mut self, index: usize) -> bool {
        let Some(&workspace) = self.visible.get(index) else {
            return false;
        };
        if workspace == self.active {
            return false;
        }
        self.active = workspace;
        true
    }

    /// Send the focused window to the workspace output `index` is showing,
    /// keeping focus where it is. Call [`Self::arrange`] afterwards.
    pub fn send_focused_to_output(&mut self, index: usize) -> bool {
        let Some(&workspace) = self.visible.get(index) else {
            return false;
        };
        self.send_focused_to_workspace(workspace)
    }

    /// Put `id` into or out of fullscreen, and report whether anything
    /// changed. Call [`Self::arrange`] afterwards to apply it.
    pub fn set_fullscreen(&mut self, id: WindowId, on: bool) -> bool {
        let output = self.output_of(id);
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        let was = window.mode;
        if on {
            window.fullscreen(output);
        } else {
            window.unfullscreen();
        }
        window.mode != was
    }

    /// Register a new window and focus it.
    ///
    /// On the active workspace, unless that workspace is already at its
    /// [`Workspace::tile_cap`] — then the window opens on the nearest
    /// workspace with room and that workspace becomes active, so the window
    /// appears in front of you rather than filing itself somewhere hidden.
    /// With every workspace full, the cap yields: a window has to exist
    /// somewhere, and an over-full workspace is a better failure than a
    /// client whose surface was never given a home.
    pub fn open_window(&mut self) -> WindowId {
        let id = WindowId::from_raw(self.next_window);
        self.next_window += 1;
        self.windows.insert(id, Window::new(id));
        if self.workspaces[self.active].is_full()
            && let Some(target) = self.workspace_with_room()
        {
            self.activate_workspace(target);
        }
        self.workspaces[self.active].insert(id);
        id
    }

    /// The workspace nearest the active one that is not at its cap, looking
    /// one step forward, then one back, then further out — so an overflowing
    /// window lands next door, not on the far end of the row.
    fn workspace_with_room(&self) -> Option<usize> {
        (1..self.workspaces.len())
            .flat_map(|step| {
                [
                    self.active
                        .checked_add(step)
                        .filter(|i| *i < self.workspaces.len()),
                    self.active.checked_sub(step),
                ]
            })
            .flatten()
            .find(|index| !self.workspaces[*index].is_full())
    }

    /// Set every workspace's window cap at once. Clamped to at least one.
    ///
    /// The compositor's way of applying a configured default; a single
    /// workspace's cap is set on the workspace itself.
    pub fn set_tile_cap(&mut self, cap: usize) {
        for workspace in &mut self.workspaces {
            workspace.set_tile_cap(cap);
        }
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
        for index in 0..self.workspaces.len() {
            if self.workspaces[index].remove(id) {
                // A soloed window that closes takes the solo with it: the
                // windows it put away come back, or the workspace would be
                // left showing nothing with no key that says why.
                if self.workspaces[index].solo() == Some(id) {
                    self.end_solo_on(index);
                }
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

    /// Move focus through panes that are actually present on screen.
    pub fn cycle_focus(&mut self, dir: Direction) -> Option<WindowId> {
        let order: Vec<_> = self.workspaces[self.active]
            .cycle_order()
            .into_iter()
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|window| !window.is_minimized())
            })
            .collect();
        if order.is_empty() {
            self.workspaces[self.active].set_focus(None);
            return None;
        }
        let current = self
            .focused()
            .and_then(|focused| order.iter().position(|id| *id == focused))
            .unwrap_or(0);
        let next = match dir {
            Direction::Forward => (current + 1) % order.len(),
            Direction::Backward => (current + order.len() - 1) % order.len(),
        };
        self.workspaces[self.active].focus(order[next]);
        Some(order[next])
    }

    /// Switch to workspace `index`. Out-of-range indices are ignored rather
    /// than clamped, so a stray keybinding cannot silently jump to workspace 9.
    ///
    /// A workspace already showing on another screen is not pulled across:
    /// focus goes to it where it is. Anything else is shown on the focused
    /// screen, replacing what that screen showed, and moves there if it lived
    /// on a different screen -- the screen you are looking at is the one you
    /// mean.
    pub fn activate_workspace(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() || index == self.active {
            return false;
        }
        if self.visible.contains(&index) {
            self.active = index;
            return true;
        }
        let output = self.focused_output();
        self.workspaces[index].set_output(output);
        self.visible[output] = index;
        self.active = index;
        true
    }

    /// The output rectangle of the screen holding window `id`, or the focused
    /// screen's when the window is on no workspace.
    fn output_of(&self, id: WindowId) -> Rect {
        let index = self
            .workspaces
            .iter()
            .find(|ws| ws.windows().contains(&id))
            .map_or(self.focused_output(), Workspace::output);
        self.outputs[index].output
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

    /// Give `id` the whole screen: put every other window on the active
    /// workspace away, remembering the tiling as it stood. The pick is not
    /// told it is fullscreen — a browser would answer by hiding its tabs and
    /// chrome. It is simply the only tile left, and a lone tile is the whole
    /// pane.
    ///
    /// The overview's pick. Soloing another window while one already holds
    /// the screen trades them without touching the remembered tiling — the
    /// state to come back to is the one from before any solo, not the one
    /// mid-solo. Returns whether `id` was here to be soloed; call
    /// [`Self::arrange`] afterwards.
    pub fn solo_window(&mut self, id: WindowId) -> bool {
        if !self.windows.contains_key(&id) || !self.workspaces[self.active].windows().contains(&id)
        {
            return false;
        }
        let members: Vec<WindowId> = self.workspaces[self.active].windows().to_vec();
        let previous = self.workspaces[self.active].solo_mut().take();
        let (tiles, mut hidden) = match previous {
            Some(prev) if prev.window == id => {
                *self.workspaces[self.active].solo_mut() = Some(prev);
                return true;
            }
            Some(prev) => (prev.tiles, prev.hidden),
            None => (self.workspaces[self.active].tiles().clone(), Vec::new()),
        };

        // The pick comes out of hiding — a previous solo may have put it
        // away. Nothing else happens to it: with the rest of the workspace
        // minimized below, the layout gives the last tile the whole pane.
        hidden.retain(|w| *w != id);
        self.unminimize(id);
        // Everyone else is put away. Only the windows put away *here* are
        // recorded, so one minimized by hand beforehand stays minimized when
        // the solo ends.
        for member in members {
            if member == id {
                continue;
            }
            let Some(window) = self.windows.get_mut(&member) else {
                continue;
            };
            if window.mode == WindowMode::Fullscreen {
                window.unfullscreen();
            }
            if !window.is_minimized() {
                window.minimize();
                if !hidden.contains(&member) {
                    hidden.push(member);
                }
            }
        }
        *self.workspaces[self.active].solo_mut() = Some(workspace::Solo {
            window: id,
            tiles,
            hidden,
        });
        self.workspaces[self.active].focus(id);
        true
    }

    /// The window soloed on the active workspace, if one is.
    pub fn solo(&self) -> Option<WindowId> {
        self.workspaces[self.active].solo()
    }

    /// Put the active workspace back the way [`Self::solo_window`] found it:
    /// the windows the solo put away come back, and the tile tree — order and
    /// moved dividers both — is the one that was remembered. Returns whether
    /// a solo was in progress; call [`Self::arrange`] afterwards.
    pub fn end_solo(&mut self) -> bool {
        self.end_solo_on(self.active)
    }

    fn end_solo_on(&mut self, index: usize) -> bool {
        let Some(solo) = self.workspaces[index].solo_mut().take() else {
            return false;
        };
        for id in solo.hidden {
            self.unminimize(id);
        }
        *self.workspaces[index].tiles_mut() = solo.tiles;
        true
    }

    /// Minimize the focused pane while leaving its client alive in this workspace.
    pub fn minimize_focused(&mut self) -> Option<WindowId> {
        let id = self.focused()?;
        self.minimize(id).then_some(id)
    }

    /// Put one window away, wherever it is. If it was the focused one, focus
    /// moves to the next window still showing on the active workspace. Call
    /// [`Self::arrange`] afterwards. Returns whether anything changed.
    pub fn minimize(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        if window.is_minimized() {
            return false;
        }
        window.minimize();
        if self.focused() != Some(id) {
            return true;
        }

        let next = self.workspaces[self.active]
            .windows()
            .iter()
            .copied()
            .find(|candidate| {
                *candidate != id
                    && self
                        .windows
                        .get(candidate)
                        .is_some_and(|window| !window.is_minimized())
            });
        self.workspaces[self.active].set_focus(next);
        true
    }

    /// Bring a minimized window back into the layout, and report whether it
    /// was minimized. Call [`Self::arrange`] afterwards to give it a tile.
    pub fn unminimize(&mut self, id: WindowId) -> bool {
        self.windows.get_mut(&id).is_some_and(Window::unminimize)
    }

    /// Move an existing window into the active workspace and focus it.
    pub fn bring_to_active_workspace(&mut self, id: WindowId) -> bool {
        if !self.windows.contains_key(&id) {
            return false;
        }
        for (index, workspace) in self.workspaces.iter_mut().enumerate() {
            if index != self.active {
                workspace.remove(id);
            }
        }
        self.workspaces[self.active].insert(id);
        self.workspaces[self.active].focus(id);
        true
    }

    /// Swap the focused window with its nearest neighbour in `dir`.
    ///
    /// Neighbours are found by geometry rather than by list order, because the
    /// two disagree the moment a layout is anything but a single row: with a
    /// master column beside a stack, "right" has to mean the window drawn to
    /// the right, whatever index it holds.
    ///
    /// Only tiled windows take part. A floating window has no slot in the order
    /// to swap, and a fullscreen one has no neighbours on screen to swap with.
    /// Returns whether anything moved; call [`Self::arrange`] afterwards to turn
    /// the new order into geometry.
    pub fn move_focused(&mut self, dir: Dir) -> bool {
        let Some(id) = self.focused() else {
            return false;
        };
        if !self.windows.get(&id).is_some_and(Window::is_tiled) {
            return false;
        }
        let Some(target) = self.neighbour(id, dir) else {
            return false;
        };
        self.workspaces[self.active].swap(id, target)
    }

    /// The tiled window nearest `from` in `dir` on the active workspace.
    fn neighbour(&self, from: WindowId, dir: Dir) -> Option<WindowId> {
        let origin = self.windows.get(&from)?.geometry;
        self.workspaces[self.active]
            .windows()
            .iter()
            .filter(|id| **id != from)
            .filter_map(|id| self.windows.get(id))
            .filter(|win| win.is_tiled())
            .filter(|win| dir.advances(origin, win.geometry) && dir.aligned(origin, win.geometry))
            // min_by_key keeps the first of equal keys, so a tie — every window
            // of a stack is equally far right of a full-height master — falls to
            // the one earliest in workspace order.
            .min_by_key(|win| dir.distance(origin, win.geometry))
            .map(Window::id)
    }

    /// Recompute geometry for every window on the active workspace.
    ///
    /// Returns only the windows whose geometry actually changed. The compositor
    /// sends one `configure` per entry, so returning the full set instead would
    /// mean a configure storm on every unrelated state change — clients treat
    /// that as a resize and re-render.
    pub fn arrange(&mut self) -> Vec<(WindowId, Rect)> {
        let mut changed = Vec::new();
        for output in 0..self.visible.len() {
            let workspace = self.visible[output];
            changed.extend(self.arrange_workspace(workspace, self.outputs[output]));
        }
        changed
    }

    /// Lay out one workspace in one screen's geometry.
    fn arrange_workspace(&mut self, index: usize, screen: OutputArea) -> Vec<(WindowId, Rect)> {
        let area = screen.area;
        let output = screen.output;
        let ws = &self.workspaces[index];

        // Everything that owns a tile — which is everything except floating
        // windows. A fullscreen window stays in the tree so that leaving
        // fullscreen puts it back exactly where it was.
        let tiled: Vec<WindowId> = ws
            .windows()
            .iter()
            .copied()
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| !w.is_floating() && !w.is_minimized())
            })
            .collect();

        // The tree is a cache over `tiled`, not a second source of truth, so
        // it is brought into line before it is read rather than trusted to
        // have been kept up to date by whoever last changed a window's mode.
        let gap = self.gap;
        let columns = self.carousel_columns;
        // A slide or a swipe belongs to the active workspace; a strip on
        // another screen simply settles where its focus asks.
        let (held, drag) = if index == self.active {
            (self.carousel_offset, self.carousel_drag)
        } else {
            (None, None)
        };
        let ws = &mut self.workspaces[index];
        let laid = match ws.layout() {
            Layout::Tiled => {
                ws.reconcile_tiles(&tiled, area);
                ws.tiles().arrange(area, gap)
            }
            // The strip needs no reconcile: it lays out `tiled` directly rather
            // than keeping a tree that has to be brought back into line with it.
            // Every window in gets a rect out, including the ones scrolled off
            // screen, which is what keeps the assertion below true for both.
            Layout::Carousel => {
                // A held offset is the compositor part way through a slide. With
                // nothing held, settle the strip here so that arranging on its
                // own — which is what the tests and any non-animating caller
                // do — still lands where focus asks for.
                let offset = match held {
                    Some(offset) => offset,
                    // A swipe has hold of it, and the compositor has not
                    // handed an offset down this frame. The fingers still win
                    // over focus, or arranging for any other reason mid-drag —
                    // a window opening, a panel resizing the area — would yank
                    // the strip out from under them.
                    None if drag == Some(ws.id()) => ws.scroll(),
                    None => {
                        let settled = strip::target_offset(
                            &tiled,
                            ws.focused(),
                            area,
                            gap,
                            columns,
                            ws.scroll(),
                        );
                        ws.set_scroll(settled);
                        settled
                    }
                };
                strip::arrange_at(&tiled, area, gap, columns, offset)
            }
        };
        debug_assert_eq!(
            laid.len(),
            tiled.len(),
            "the layout and the tiled set disagree"
        );

        let mut changed = Vec::new();
        for (id, rect) in laid {
            let Some(win) = self.windows.get_mut(&id) else {
                continue;
            };
            // Holds a tile but is not currently in it; the fullscreen pass
            // below gives it the whole area.
            if !win.is_tiled() {
                continue;
            }
            // A tile is an allocation, not a size suggestion. Honouring a
            // client's minimum or maximum here makes its recorded geometry
            // differ from the split tree: a large minimum overlaps a sibling,
            // and a small maximum leaves a hole. The client may still commit a
            // differently sized buffer, but the compositor centres and clips
            // that buffer inside this authoritative pane.
            if win.geometry != rect {
                win.geometry = rect;
                changed.push((id, rect));
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
                WindowMode::Fullscreen => output,
                WindowMode::Floating => win.geometry.constrain_to(area),
                WindowMode::Minimized => continue,
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
    use crate::geometry::{Dir, Point};
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
    fn a_frame_inset_does_not_change_what_arrange_reports() {
        let mut s = space();
        let a = s.open_window();
        s.open_window();
        s.arrange();
        let pane = s.window(a).expect("open").geometry;

        s.window_mut(a).expect("open").frame_top = 30;
        assert!(
            s.arrange().is_empty(),
            "the frame is the window's own business, not the layout's"
        );
        let w = s.window(a).expect("open");
        assert_eq!(w.geometry, pane, "the pane is unchanged");
        assert_eq!(w.content().h(), pane.h() - 30);
        assert_eq!(w.content().y(), pane.y() + 30);
        assert_eq!(w.content().w(), pane.w());
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
            assert!(
                g.x() >= SCREEN.x() && g.right() <= SCREEN.right(),
                "{g:?} escaped horizontally"
            );
            assert!(
                g.y() >= SCREEN.y() && g.bottom() <= SCREEN.bottom(),
                "{g:?} escaped vertically"
            );
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
        assert!(
            g.right() <= 800 && g.bottom() <= 600,
            "{g:?} left the screen"
        );
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
    fn tiled_layout_stays_authoritative_over_client_size_hints() {
        let mut s = space();
        let w = s.open_window();
        s.window_mut(w).expect("just opened").hints = SizeHints {
            min: None,
            max: Some(Size::new(640, 480)),
        };
        s.arrange();
        assert_eq!(
            s.window(w).expect("still open").geometry,
            SCREEN.inset(DEFAULT_GAP),
            "a client hint must not make its pane leave a hole"
        );
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

        s.window_mut(a).expect("open").unfullscreen();
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, tiled_geom);
    }

    #[test]
    fn fullscreen_covers_the_output_not_the_area_between_panels() {
        let mut s = space();
        // A 40px top panel takes an exclusive zone; windows tile beneath it.
        s.set_area(Rect::from_xywh(0, 40, 1920, 1040));
        let a = s.open_window();
        s.open_window();
        s.arrange();
        let tiled_geom = s.window(a).expect("open").geometry;
        assert!(tiled_geom.y() >= 40, "tiled under the panel");

        assert!(s.set_fullscreen(a, true));
        s.arrange();
        assert_eq!(
            s.window(a).expect("open").geometry,
            SCREEN,
            "fullscreen is the whole screen, panel included"
        );
        assert!(!s.set_fullscreen(a, true), "already fullscreen");

        assert!(s.set_fullscreen(a, false));
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, tiled_geom);
    }

    #[test]
    fn a_resized_output_moves_the_fullscreen_window_with_it() {
        let mut s = space();
        let a = s.open_window();
        s.arrange();
        s.set_fullscreen(a, true);
        s.arrange();
        let bigger = Rect::from_xywh(0, 0, 2560, 1440);
        s.set_output(bigger);
        s.set_area(bigger);
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, bigger);
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

    /// Three windows in the canonical shape: `a` and `b` side by side on top,
    /// `c` across the whole bottom at the same height.
    fn a_b_c() -> (Space, WindowId, WindowId, WindowId) {
        let mut s = space();
        let a = s.open_window();
        let b = s.open_window();
        let c = s.open_window();
        s.arrange();
        (s, a, b, c)
    }

    #[test]
    fn three_windows_take_the_two_over_one_shape() {
        let (s, a, b, c) = a_b_c();
        let rect = |id| s.window(id).expect("open").geometry;
        let (ra, rb, rc) = (rect(a), rect(b), rect(c));
        assert_eq!(ra.y(), rb.y(), "a and b share the top row");
        assert!(ra.x() < rb.x());
        assert_eq!(ra.h(), rb.h());
        assert!(rc.y() > ra.y(), "c sits beneath them");
        assert!(rc.w() > ra.w() + rb.w(), "c spans the full width");
        assert!(ra.h().abs_diff(rc.h()) <= 1, "the rows are equal heights");
    }

    #[test]
    fn moving_right_swaps_the_top_pair() {
        let (mut s, a, b, c) = a_b_c();
        s.active_workspace_mut().focus(a);
        assert!(s.move_focused(Dir::Right));
        // Tile order, not membership order: membership records who lives here,
        // the tree records where they sit, and only the latter moves.
        assert_eq!(s.active_workspace().tiles().windows(), [b, a, c]);
        assert_eq!(s.focused(), Some(a), "the window moved, not the focus");
    }

    #[test]
    fn moving_down_lands_in_the_wide_tile() {
        let (mut s, a, b, c) = a_b_c();
        s.active_workspace_mut().focus(b);
        assert!(s.move_focused(Dir::Down));
        assert_eq!(s.active_workspace().tiles().windows(), [a, c, b]);

        // From the wide tile both top tiles are equally near; the tie falls
        // to the one earliest in workspace order.
        s.arrange();
        assert!(s.move_focused(Dir::Up));
        assert_eq!(s.active_workspace().tiles().windows(), [b, c, a]);
    }

    #[test]
    fn moving_into_the_wall_does_nothing() {
        let (mut s, a, _b, _c) = a_b_c();
        s.active_workspace_mut().focus(a);
        // The top-left tile has nothing above it or to its left.
        for dir in [Dir::Left, Dir::Up] {
            assert!(
                !s.move_focused(dir),
                "{dir:?} found a neighbour that is not there"
            );
        }
        assert_eq!(s.active_workspace().windows()[0], a);
    }

    #[test]
    fn a_lone_window_has_nowhere_to_move() {
        let mut s = space();
        s.open_window();
        s.arrange();
        assert!(!s.move_focused(Dir::Right));
    }

    #[test]
    fn moving_with_nothing_focused_is_a_no_op() {
        let mut s = space();
        assert!(!s.move_focused(Dir::Left));
    }

    #[test]
    fn floating_windows_do_not_join_the_tiling_order() {
        let (mut s, a, b, _c) = a_b_c();
        s.window_mut(b).expect("open").mode = WindowMode::Floating;
        s.arrange();
        s.active_workspace_mut().focus(b);
        assert!(
            !s.move_focused(Dir::Left),
            "a floating window has no slot to swap"
        );

        // And it is not a target either: moving right out of the first tile
        // must skip it.
        s.active_workspace_mut().focus(a);
        assert!(s.move_focused(Dir::Right));
        assert_ne!(s.active_workspace().windows()[0], b);
    }

    #[test]
    fn a_move_is_its_own_inverse() {
        let (mut s, a, b, c) = a_b_c();
        s.active_workspace_mut().focus(c);
        assert!(s.move_focused(Dir::Up));
        s.arrange();
        assert!(s.move_focused(Dir::Down));
        assert_eq!(s.active_workspace().windows(), &[a, b, c]);
    }

    #[test]
    fn soloing_gives_one_window_the_screen_and_puts_the_rest_away() {
        let (mut s, a, b, c) = a_b_c();
        assert!(s.solo_window(a));
        s.arrange();
        assert_eq!(s.solo(), Some(a));
        assert_eq!(s.focused(), Some(a));
        assert_eq!(
            s.window(a).unwrap().geometry,
            SCREEN.inset(DEFAULT_GAP),
            "a is the lone pane, not app fullscreen"
        );
        assert!(
            s.window(a).unwrap().is_tiled(),
            "the solo must not tell the client it is fullscreen"
        );
        assert!(s.window(b).unwrap().is_minimized());
        assert!(s.window(c).unwrap().is_minimized());
    }

    #[test]
    fn ending_the_solo_restores_the_tiling_as_it_stood() {
        // Not merely re-tiled: the order after a swap and a moved divider
        // both come back, because "put them back" means where they were.
        let (mut s, a, b, c) = a_b_c();
        s.active_workspace_mut().focus(a);
        assert!(s.move_focused(Dir::Right)); // tiles now [b, a, c]
        s.arrange();
        let before: Vec<Rect> = [a, b, c]
            .iter()
            .map(|id| s.window(*id).unwrap().geometry)
            .collect();

        assert!(s.solo_window(c));
        s.arrange();
        assert!(s.end_solo());
        s.arrange();

        assert_eq!(s.solo(), None);
        assert_eq!(s.active_workspace().tiles().windows(), [b, a, c]);
        for (id, geometry) in [a, b, c].iter().zip(before) {
            assert_eq!(
                s.window(*id).unwrap().geometry,
                geometry,
                "a window did not return to its tile"
            );
            assert!(!s.window(*id).unwrap().is_minimized());
        }
    }

    #[test]
    fn trading_the_solo_keeps_the_original_tiling_to_come_back_to() {
        let (mut s, a, b, c) = a_b_c();
        s.arrange();
        let original = s.window(a).unwrap().geometry;

        assert!(s.solo_window(a));
        s.arrange();
        assert!(s.solo_window(b), "the pick can move while soloed");
        s.arrange();
        assert_eq!(s.solo(), Some(b));
        assert_eq!(s.window(b).unwrap().geometry, SCREEN.inset(DEFAULT_GAP));
        assert!(
            s.window(a).unwrap().is_minimized(),
            "the old pick steps back"
        );

        assert!(s.end_solo());
        s.arrange();
        assert_eq!(
            s.window(a).unwrap().geometry,
            original,
            "the restore is the tiling before any solo"
        );
        assert!(
            [a, b, c]
                .iter()
                .all(|id| !s.window(*id).unwrap().is_minimized())
        );
    }

    #[test]
    fn a_window_minimized_by_hand_stays_minimized_after_the_solo() {
        let (mut s, a, b, _c) = a_b_c();
        s.minimize(b);
        s.arrange();
        assert!(s.solo_window(a));
        s.arrange();
        assert!(s.end_solo());
        s.arrange();
        assert!(
            s.window(b).unwrap().is_minimized(),
            "the solo must only bring back what it put away"
        );
    }

    #[test]
    fn closing_the_soloed_window_brings_the_others_back() {
        let (mut s, a, b, c) = a_b_c();
        assert!(s.solo_window(a));
        s.arrange();
        assert!(s.close_window(a));
        s.arrange();
        assert_eq!(s.solo(), None);
        assert!(!s.window(b).unwrap().is_minimized());
        assert!(!s.window(c).unwrap().is_minimized());
    }

    #[test]
    fn soloing_the_same_window_twice_changes_nothing() {
        let (mut s, a, _b, _c) = a_b_c();
        assert!(s.solo_window(a));
        assert!(s.solo_window(a));
        assert!(s.end_solo());
        assert!(!s.end_solo(), "the solo ended the first time");
    }

    #[test]
    fn a_window_somewhere_else_cannot_be_soloed() {
        let mut s = space();
        let a = s.open_window();
        s.activate_workspace(1);
        assert!(!s.solo_window(a));
        assert!(!s.solo_window(WindowId::from_raw(999)));
        assert_eq!(s.solo(), None);
    }

    #[test]
    fn the_ninth_window_overflows_to_the_next_workspace() {
        let mut s = space();
        for _ in 0..workspace::DEFAULT_TILE_CAP {
            s.open_window();
        }
        assert_eq!(s.active_index(), 0, "eight windows fit where they opened");

        let ninth = s.open_window();
        assert_eq!(s.active_index(), 1, "the ninth moves you next door");
        assert_eq!(s.workspaces()[1].windows(), &[ninth]);
        assert_eq!(s.focused(), Some(ninth), "and arrives focused");
        assert_eq!(
            s.workspaces()[0].windows().len(),
            workspace::DEFAULT_TILE_CAP,
            "the full workspace was left as it was"
        );
    }

    #[test]
    fn overflow_finds_the_nearest_workspace_with_room() {
        // Forward first, then backward, then further out — so with the next
        // workspace also full, the window lands one step back instead.
        let mut s = space();
        s.set_tile_cap(1);
        s.open_window();
        s.open_window(); // fills workspace 2, which becomes active
        assert_eq!(s.active_index(), 1);
        s.open_window(); // 1 and 2 are full; next door forward is 3
        assert_eq!(s.active_index(), 2);

        // Back on the full first workspace with 2 and 3 also full, one step
        // forward is taken before two steps could be.
        s.activate_workspace(0);
        s.open_window();
        assert_eq!(s.active_index(), 3);
    }

    #[test]
    fn with_every_workspace_full_the_cap_yields() {
        // A window must exist somewhere; an over-full workspace beats a
        // client with no home.
        let mut s = space();
        s.set_tile_cap(1);
        for _ in 0..9 {
            s.open_window();
        }
        assert!(s.workspaces().iter().all(Workspace::is_full));
        let extra = s.open_window();
        assert!(
            s.active_workspace().windows().contains(&extra),
            "the tenth window still landed on the active workspace"
        );
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

    #[test]
    fn a_workspace_starts_tiled_and_toggles_to_the_carousel() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        assert_eq!(s.active_workspace().layout(), Layout::Tiled);
        assert_eq!(s.toggle_layout(), Layout::Carousel);
        assert_eq!(s.toggle_layout(), Layout::Tiled);
    }

    #[test]
    fn the_carousel_keeps_pane_size_as_windows_arrive_and_tiling_does_not() {
        // The difference the mode exists for, stated as the property rather
        // than as one arithmetic coincidence: adding windows to a tiled
        // workspace makes the existing ones smaller, and adding them to a
        // carousel does not — the strip grows past the edge instead.
        // The newest window is the probe: each `insert` splits the tile the
        // *focused* window holds, and opening focuses what you opened, so the
        // first window is split once and then left alone while the newest keeps
        // being halved. Measuring the first would show tiling holding steady.
        let size_after = |layout: Layout, count: usize| {
            let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
            s.active_workspace_mut().set_layout(layout);
            let mut newest = s.open_window();
            for _ in 1..count {
                newest = s.open_window();
            }
            s.arrange();
            let g = s.window(newest).unwrap().geometry;
            (g.w(), g.h())
        };

        assert_eq!(
            size_after(Layout::Carousel, 2),
            size_after(Layout::Carousel, 8),
            "a carousel pane is the same size whether two windows are open or eight"
        );
        assert_ne!(
            size_after(Layout::Tiled, 2),
            size_after(Layout::Tiled, 8),
            "tiling divides the same area further as windows arrive"
        );
    }

    #[test]
    fn a_tiled_workspace_has_no_carousel_offset_to_animate() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        s.open_window();
        assert_eq!(s.update_carousel_target(), None);
    }

    #[test]
    fn each_workspace_keeps_its_own_scroll_position() {
        // Scroll is per workspace, like the layout it belongs to. Held globally,
        // arriving on a carousel would inherit wherever the last one happened to
        // be sitting.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let mut first = Vec::new();
        for _ in 0..6 {
            first.push(s.open_window());
        }
        s.toggle_layout();
        s.active_workspace_mut().focus(first[5]);
        let scrolled = s.update_carousel_target().expect("a carousel has a target");
        assert!(
            scrolled > 0,
            "the first workspace is scrolled along its strip"
        );

        // A second carousel workspace, with its own short strip.
        assert!(s.activate_workspace(1));
        s.toggle_layout();
        s.open_window();
        assert_eq!(
            s.update_carousel_target(),
            Some(0),
            "a fresh workspace starts at its own beginning, not the last one's"
        );

        // And going back finds the first one where it was left.
        assert!(s.activate_workspace(0));
        assert_eq!(s.update_carousel_target(), Some(scrolled));
    }

    #[test]
    fn toggling_out_to_tiling_and_back_keeps_the_scroll() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let mut opened = Vec::new();
        for _ in 0..6 {
            opened.push(s.open_window());
        }
        s.toggle_layout();
        s.active_workspace_mut().focus(opened[5]);
        let scrolled = s.update_carousel_target().expect("a carousel has a target");

        s.toggle_layout();
        assert_eq!(s.update_carousel_target(), None, "tiling has no strip");
        s.toggle_layout();
        assert_eq!(
            s.update_carousel_target(),
            Some(scrolled),
            "the strip is where it was, not back at its start"
        );
    }

    #[test]
    fn the_held_offset_slides_the_strip_without_moving_the_target() {
        // The animation contract. The compositor holds the strip part way while
        // it slides; the target stays where focus is, so releasing the hold puts
        // it exactly where it was always heading.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let mut opened = Vec::new();
        for _ in 0..6 {
            opened.push(s.open_window());
        }
        s.toggle_layout();
        s.active_workspace_mut().focus(opened[5]);

        let target = s.update_carousel_target().expect("a carousel has a target");
        assert!(target > 0, "focus on the last pane must scroll the strip");

        s.set_carousel_offset(Some(target / 2));
        s.arrange();
        let midway = s.window(opened[0]).unwrap().geometry.x();

        assert_eq!(
            s.update_carousel_target(),
            Some(target),
            "holding the strip part way must not move where it is going"
        );

        s.set_carousel_offset(None);
        s.arrange();
        let arrived = s.window(opened[0]).unwrap().geometry.x();

        assert!(arrived < midway, "releasing the hold completes the scroll");
        assert_eq!(arrived, midway - (target - target / 2));
    }

    #[test]
    fn a_held_offset_does_not_leak_into_a_tiled_workspace() {
        // `set_carousel_offset` is unconditional, so the compositor clearing it
        // when the layout is not a carousel is what keeps a stale slide from
        // displacing tiled windows. Guard the core side of that too.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let a = s.open_window();
        s.arrange();
        let tiled = s.window(a).unwrap().geometry;

        s.set_carousel_offset(Some(400));
        s.arrange();
        assert_eq!(
            s.window(a).unwrap().geometry,
            tiled,
            "a tiled workspace ignores the carousel's offset"
        );
    }

    #[test]
    fn going_to_the_carousel_and_back_restores_the_tiling() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let a = s.open_window();
        let b = s.open_window();
        s.arrange();
        let before = (s.window(a).unwrap().geometry, s.window(b).unwrap().geometry);

        s.toggle_layout();
        s.arrange();
        s.toggle_layout();
        s.arrange();

        let after = (s.window(a).unwrap().geometry, s.window(b).unwrap().geometry);
        assert_eq!(before, after, "the tile tree survived the round trip");
    }

    /// A carousel workspace with `n` panes on a 1000x600 screen, focused on the
    /// first, with the strip resting at its start.
    ///
    /// Returns the column stride alongside, read off the geometry rather than
    /// recomputed, so a test that swipes "two panes across" cannot disagree
    /// with the layout about how far that is.
    fn strip_of(n: usize) -> (Space, Vec<WindowId>, i32) {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let windows: Vec<WindowId> = (0..n).map(|_| s.open_window()).collect();
        if let Some(first) = windows.first() {
            s.active_workspace_mut().focus(*first);
        }
        s.toggle_layout();
        s.arrange();
        let stride = match windows.get(1) {
            Some(second) => {
                s.window(*second).unwrap().geometry.x() - s.window(windows[0]).unwrap().geometry.x()
            }
            None => 0,
        };
        (s, windows, stride)
    }

    #[test]
    fn a_swipe_turns_a_tiled_workspace_into_the_carousel_and_leaves_it_there() {
        // The gesture is how you get to the carousel, so it makes the same
        // change the keybinding does — including outlasting the fingers.
        //
        // Three panes, not two: at the default two columns a pair already
        // fills the viewport, and a swipe there is refused precisely because
        // it could change nothing visible. See the test below.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        s.open_window();
        s.open_window();
        s.open_window();
        assert_eq!(s.active_workspace().layout(), Layout::Tiled);

        s.begin_carousel_drag()
            .expect("three panes to take hold of");
        assert_eq!(s.active_workspace().layout(), Layout::Carousel);

        s.end_carousel_drag();
        assert_eq!(
            s.active_workspace().layout(),
            Layout::Carousel,
            "the layout is a decision, not a thing the fingers hold open"
        );
    }

    #[test]
    fn a_swipe_over_a_workspace_that_already_fits_changes_nothing() {
        // The invisible-flip bug: one or two panes at two columns lay out
        // identically tiled and carousel, and the strip cannot scroll at all,
        // so taking the claim would leave the workspace silently in a mode the
        // screen gives no sign of — and the next Super+Ctrl+C would look
        // inverted, toggling back to tiling instead of into the carousel.
        for panes in [1, 2] {
            let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
            for _ in 0..panes {
                s.open_window();
            }
            assert_eq!(
                strip::max_offset(&s.tiled_windows(), s.area(), s.gap, s.carousel_columns),
                0,
                "{panes} panes must have nothing to scroll"
            );
            assert_eq!(
                s.begin_carousel_drag(),
                None,
                "{panes} panes: a swipe that can do nothing visible must be refused"
            );
            assert_eq!(
                s.active_workspace().layout(),
                Layout::Tiled,
                "{panes} panes: the layout must not flip under a refused swipe"
            );
            assert!(!s.carousel_dragging());
        }
    }

    #[test]
    fn a_swipe_is_taken_as_soon_as_the_strip_can_actually_move() {
        // The other half of the rule: refusing what cannot scroll must not
        // turn into refusing a strip that can. A third pane is what makes the
        // strip longer than the viewport at two columns.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        s.open_window();
        s.open_window();
        assert_eq!(s.begin_carousel_drag(), None, "two panes still fit");

        s.open_window();
        assert!(
            s.begin_carousel_drag().is_some(),
            "a third pane gives the strip somewhere to go"
        );
        assert_eq!(s.active_workspace().layout(), Layout::Carousel);
    }

    #[test]
    fn a_swipe_over_an_empty_workspace_changes_nothing() {
        // Nothing to take hold of. Flipping the layout anyway would be a mode
        // change with no visible cause, discovered later on an empty screen.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        assert_eq!(s.begin_carousel_drag(), None);
        assert_eq!(s.active_workspace().layout(), Layout::Tiled);
        assert!(!s.carousel_dragging());
    }

    #[test]
    fn the_strip_follows_the_fingers_rather_than_the_focused_pane() {
        // The whole point of the drag flag: focus is still on pane 1, and the
        // strip must be allowed to leave it off screen until the fingers lift.
        let (mut s, windows, _) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        assert_eq!(s.drag_carousel(700), Some(700));
        assert_eq!(
            s.update_carousel_target(),
            Some(700),
            "focus does not pull it back"
        );

        s.arrange();
        let first = s.window(windows[0]).unwrap().geometry;
        assert!(
            first.right() < 0,
            "pane 1 is off screen while the drag holds it there"
        );
    }

    #[test]
    fn a_drag_stops_at_the_ends_of_the_strip() {
        let (mut s, windows, stride) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");

        assert_eq!(s.drag_carousel(-4000), Some(0), "it stops at the start");
        let end = s
            .drag_carousel(99_999)
            .expect("the drag holds this workspace");
        assert!(
            end > 0 && end < 5 * stride,
            "the end is the last pane flush, not past it"
        );

        // Which is to say: at the end, the final pane sits at the right edge.
        s.arrange();
        assert_eq!(s.window(windows[5]).unwrap().geometry.right(), 992);
    }

    #[test]
    fn letting_go_settles_on_a_pane_and_focuses_it() {
        let (mut s, windows, stride) = strip_of(6);
        assert_eq!(s.focused(), Some(windows[0]));

        s.begin_carousel_drag().expect("panes to take hold of");
        // Two full columns across, so the third pane is what the fingers left
        // at the left of the viewport.
        s.drag_carousel(stride * 2);
        let settled = s.end_carousel_drag().expect("a pane to settle on");

        assert_eq!(settled, windows[2]);
        assert_eq!(
            s.focused(),
            Some(windows[2]),
            "the keyboard went where the fingers did"
        );
        assert!(!s.carousel_dragging(), "the strip was let go of");
    }

    #[test]
    fn a_settled_strip_stays_where_the_swipe_left_it() {
        // The regression this guards: the drag moves the strip, focus stays
        // put, and the next arrange snaps it straight back. Focusing the pane
        // it settled on is what stops that, and this is the proof.
        let (mut s, _, stride) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        // Two columns across and a few pixels short of it, so the settle has
        // somewhere to round to and the test is not asserting on a no-op.
        s.drag_carousel(stride * 2 - 6);
        s.end_carousel_drag().expect("a pane to settle on");

        let after = s.update_carousel_target().expect("a carousel has a target");
        assert_eq!(
            after,
            stride * 2,
            "the arrange after the swipe leaves it alone"
        );
        // And again, because the nudge rule reads the position it last wrote.
        assert_eq!(s.update_carousel_target(), Some(stride * 2));
    }

    #[test]
    fn a_swipe_that_barely_moves_settles_back_where_it_started() {
        let (mut s, windows, _) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        s.drag_carousel(20);
        assert_eq!(s.end_carousel_drag(), Some(windows[0]));
        assert_eq!(
            s.update_carousel_target(),
            Some(0),
            "back to the start, not stuck at 20"
        );
    }

    #[test]
    fn switching_workspace_mid_swipe_lets_go_rather_than_freezing_the_new_one() {
        // The drag is held by workspace id precisely so this cannot strand the
        // strip you arrive on outside the focus rule that governs it.
        let (mut s, _, _) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        s.drag_carousel(600);

        s.activate_workspace(1);
        assert!(
            !s.carousel_dragging(),
            "the swipe does not follow you across"
        );
        assert_eq!(
            s.drag_carousel(900),
            None,
            "and it cannot move the strip here"
        );
        assert_eq!(s.end_carousel_drag(), None);
    }

    #[test]
    fn a_swipe_never_settles_a_pane_the_strip_does_not_show() {
        // The pairing `strip::snap` promises, exercised through the whole path
        // rather than in the layout alone.
        for stop in [0, 130, 495, 700, 1200, 9_000] {
            let (mut s, _, _) = strip_of(7);
            s.begin_carousel_drag().expect("panes to take hold of");
            s.drag_carousel(stop);
            let pane = s.end_carousel_drag().expect("a pane to settle on");
            s.arrange();
            let geometry = s.window(pane).unwrap().geometry;
            assert!(
                geometry.x() >= 0 && geometry.right() <= 1000,
                "letting go at {stop} focused a pane at {geometry:?}, off screen"
            );
        }
    }

    #[test]
    fn unminimizing_returns_the_pane_to_the_layout_as_a_tile() {
        let mut s = space();
        let first = s.open_window();
        let second = s.open_window();
        s.arrange();
        assert_eq!(s.minimize_focused(), Some(second));
        s.arrange();

        assert!(s.unminimize(second));
        assert!(!s.unminimize(second), "already restored");
        s.bring_to_active_workspace(second);
        s.arrange();
        let restored = s.window(second).unwrap();
        assert!(restored.is_tiled(), "restore is a tile, not fullscreen");
        assert_ne!(restored.geometry, s.output(), "must not cover the output");
        assert!(s.window(first).unwrap().geometry.right() <= restored.geometry.x());
        assert_eq!(s.focused(), Some(second));
    }

    #[test]
    fn minimizing_removes_only_the_focused_pane_from_layout() {
        let mut s = space();
        let first = s.open_window();
        let second = s.open_window();
        s.arrange();

        assert_eq!(s.minimize_focused(), Some(second));
        let changed = s.arrange();
        assert!(s.window(second).unwrap().is_minimized());
        assert_eq!(s.focused(), Some(first));
        assert_eq!(
            s.window(first).unwrap().geometry,
            Rect::from_xywh(8, 8, 1904, 1064)
        );
        assert!(changed.iter().all(|(id, _)| *id != second));
    }

    #[test]
    fn a_minimized_pane_can_be_brought_to_the_current_workspace() {
        let mut s = space();
        let pane = s.open_window();
        s.minimize_focused();
        s.activate_workspace(2);

        assert!(s.bring_to_active_workspace(pane));
        assert_eq!(s.active_workspace().windows(), &[pane]);
        assert_eq!(s.focused(), Some(pane));
        assert!(!s.workspaces()[0].windows().contains(&pane));
    }

    // ---- outputs -------------------------------------------------------

    const LEFT: Rect = Rect::from_xywh(0, 0, 1920, 1080);
    const RIGHT: Rect = Rect::from_xywh(1920, 0, 2560, 1440);

    fn two_screens() -> Space {
        let mut s = Space::new(LEFT);
        s.set_outputs(vec![OutputArea::new(LEFT), OutputArea::new(RIGHT)]);
        s
    }

    #[test]
    fn a_new_output_is_given_a_workspace_of_its_own() {
        let s = two_screens();
        let visible: Vec<_> = s.visible_workspaces().collect();
        assert_eq!(visible, vec![(0, 0), (1, 1)]);
        assert_eq!(s.workspaces()[1].output(), 1);
        assert_eq!(s.focused_output(), 0, "focus stays on the screen it was on");
    }

    #[test]
    fn each_screen_lays_its_workspace_out_in_its_own_area() {
        let mut s = two_screens();
        let left = s.open_window();
        s.activate_workspace(1);
        let right = s.open_window();
        let changed = s.arrange();
        assert_eq!(changed.len(), 2);
        assert!(
            LEFT.contains(s.window(left).unwrap().geometry.center()),
            "a window on workspace 1 tiles on the left screen"
        );
        assert!(
            RIGHT.contains(s.window(right).unwrap().geometry.center()),
            "a window on workspace 2 tiles on the right screen"
        );
        assert!(
            !s.window(right).unwrap().geometry.overlaps(LEFT),
            "nothing straddles the bezel"
        );
    }

    #[test]
    fn activating_a_workspace_shown_on_another_screen_moves_focus_there() {
        let mut s = two_screens();
        assert!(s.activate_workspace(1));
        assert_eq!(s.focused_output(), 1);
        // It did not get pulled across: the left screen still shows workspace 1.
        assert_eq!(s.visible_on(0), Some(0));
        assert_eq!(s.visible_on(1), Some(1));
    }

    #[test]
    fn activating_a_hidden_workspace_shows_it_on_the_focused_screen() {
        let mut s = two_screens();
        s.focus_output(1);
        assert!(s.activate_workspace(4));
        assert_eq!(s.visible_on(1), Some(4));
        assert_eq!(s.workspaces()[4].output(), 1);
        assert_eq!(s.visible_on(0), Some(0), "the other screen is untouched");
    }

    #[test]
    fn fullscreen_covers_the_window_s_own_screen() {
        let mut s = two_screens();
        s.focus_output(1);
        let id = s.open_window();
        s.set_fullscreen(id, true);
        s.arrange();
        assert_eq!(s.window(id).unwrap().geometry, RIGHT);
    }

    #[test]
    fn sending_to_an_output_moves_the_window_and_keeps_focus() {
        let mut s = two_screens();
        let id = s.open_window();
        assert!(s.send_focused_to_output(1));
        s.arrange();
        assert!(RIGHT.contains(s.window(id).unwrap().geometry.center()));
        assert_eq!(s.focused_output(), 0);
    }

    #[test]
    fn unplugging_a_screen_brings_its_workspaces_home() {
        let mut s = two_screens();
        s.focus_output(1);
        let id = s.open_window();
        s.set_outputs(vec![OutputArea::new(LEFT)]);
        assert_eq!(s.outputs().len(), 1);
        assert_eq!(s.workspaces()[1].output(), 0);
        // The workspace being worked in stays the one on screen.
        assert_eq!(s.visible_on(0), Some(1));
        assert_eq!(s.focused_output(), 0);
        s.arrange();
        assert!(LEFT.contains(s.window(id).unwrap().geometry.center()));
    }

    #[test]
    fn replugging_restores_a_desktop_on_the_new_screen() {
        let mut s = two_screens();
        s.set_outputs(vec![OutputArea::new(LEFT)]);
        s.set_outputs(vec![OutputArea::new(LEFT), OutputArea::new(RIGHT)]);
        let visible: Vec<_> = s.visible_workspaces().collect();
        assert_eq!(visible.len(), 2);
        assert_ne!(visible[0].1, visible[1].1, "no workspace shows twice");
    }

    #[test]
    fn a_panel_on_one_screen_reserves_nothing_on_the_other() {
        let mut s = two_screens();
        assert!(s.set_output_area(0, Rect::from_xywh(0, 40, 1920, 1040)));
        assert_eq!(s.outputs()[1].area, RIGHT);
        assert_eq!(s.area(), Rect::from_xywh(0, 40, 1920, 1040));
    }

    #[test]
    fn output_at_finds_the_screen_under_a_point() {
        let s = two_screens();
        assert_eq!(s.output_at(geometry::Point::new(10, 10)), Some(0));
        assert_eq!(s.output_at(geometry::Point::new(3000, 10)), Some(1));
        assert_eq!(s.output_at(geometry::Point::new(10, 2000)), None);
    }

    #[test]
    fn an_empty_output_list_is_ignored() {
        let mut s = two_screens();
        s.set_outputs(Vec::new());
        assert_eq!(s.outputs().len(), 2);
    }
}
