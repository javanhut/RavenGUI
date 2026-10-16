//! The clipboard and the primary selection, shared with X11 clients.
//!
//! XWayland does not take part in `wl_data_device` or the primary-selection
//! protocol for its X11 clients; the window manager is expected to carry
//! selections across. smithay's `X11Wm` does the X side of a transfer — it
//! owns `CLIPBOARD` and `PRIMARY` on Wayland's behalf, and reads an X
//! client's selection into a pipe — and this module decides which way each
//! change goes:
//!
//! - A Wayland client sets a selection: XWayland is told its mime types and
//!   claims the X selection for it. An X client pasting reads it through
//!   `XwmHandler::send_selection`, which asks the Wayland source to write.
//! - An X client sets a selection: it becomes the seat's selection as a
//!   compositor-owned one with the X client's mime types. A Wayland client
//!   pasting reads it through `SelectionHandler::send_selection`, which asks
//!   XWayland to write.
//!
//! Setting a selection from the compositor does not call
//! `SelectionHandler::new_selection`, and XWayland ignores its own claims, so
//! neither direction echoes back into the other.

use std::os::unix::io::OwnedFd;

use smithay::{
    wayland::selection::SelectionTarget,
    xwayland::{X11Wm, xwm::SelectionError},
};

/// Starts a transfer of the X selection into `fd` on the event loop.
pub(crate) type Sender =
    Box<dyn Fn(&mut X11Wm, SelectionTarget, String, OwnedFd) -> Result<(), SelectionError>>;

/// Where a selection last came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Owner {
    #[default]
    Nobody,
    /// A Wayland client, offering these mime types.
    Wayland(Vec<String>),
    /// An X11 client; the seat holds a compositor-owned stand-in for it.
    X11,
}

/// What the compositor has to do to keep both sides in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Nothing,
    /// Tell XWayland the Wayland selection changed. `None` withdraws the
    /// claim XWayland holds for us.
    OfferToX11(Option<Vec<String>>),
    /// Make an X client's selection the seat's, with these mime types.
    SetWayland(Vec<String>),
    /// Withdraw the seat's selection.
    ClearWayland,
}

const TARGETS: [SelectionTarget; 2] = [SelectionTarget::Clipboard, SelectionTarget::Primary];

/// Who owns the clipboard and who owns the primary selection.
#[derive(Debug, Default)]
pub(crate) struct Bridge {
    clipboard: Owner,
    primary: Owner,
}

impl Bridge {
    fn slot(&mut self, target: SelectionTarget) -> &mut Owner {
        match target {
            SelectionTarget::Clipboard => &mut self.clipboard,
            SelectionTarget::Primary => &mut self.primary,
        }
    }

    pub(crate) fn owner(&self, target: SelectionTarget) -> &Owner {
        match target {
            SelectionTarget::Clipboard => &self.clipboard,
            SelectionTarget::Primary => &self.primary,
        }
    }

    /// A Wayland client set the selection, or cleared it with `None`.
    pub(crate) fn wayland_selection(
        &mut self,
        target: SelectionTarget,
        mime_types: Option<Vec<String>>,
    ) -> Action {
        match mime_types.filter(|types| !types.is_empty()) {
            Some(types) => {
                *self.slot(target) = Owner::Wayland(types.clone());
                Action::OfferToX11(Some(types))
            }
            None => {
                *self.slot(target) = Owner::Nobody;
                Action::OfferToX11(None)
            }
        }
    }

    /// An X11 client took the selection, offering these mime types.
    pub(crate) fn x11_selection(
        &mut self,
        target: SelectionTarget,
        mime_types: Vec<String>,
    ) -> Action {
        let slot = self.slot(target);
        if !mime_types.is_empty() {
            *slot = Owner::X11;
            return Action::SetWayland(mime_types);
        }
        // Nothing it offers can be named to a Wayland client. What the seat
        // held before is no longer the latest copy, so it goes too.
        match std::mem::take(slot) {
            Owner::Nobody => Action::Nothing,
            Owner::Wayland(_) | Owner::X11 => Action::ClearWayland,
        }
    }

    /// The X11 client that owned the selection let it go.
    pub(crate) fn x11_cleared(&mut self, target: SelectionTarget) -> Action {
        let slot = self.slot(target);
        if *slot == Owner::X11 {
            *slot = Owner::Nobody;
            Action::ClearWayland
        } else {
            Action::Nothing
        }
    }

    /// XWayland went away: every selection standing in for an X client has
    /// nothing behind it any more.
    pub(crate) fn x11_gone(&mut self) -> Vec<SelectionTarget> {
        TARGETS
            .into_iter()
            .filter(|&target| self.x11_cleared(target) == Action::ClearWayland)
            .collect()
    }

    /// What a newly started XWayland has to be told: the selections Wayland
    /// clients already hold.
    pub(crate) fn replay(&self) -> Vec<(SelectionTarget, Vec<String>)> {
        TARGETS
            .into_iter()
            .filter_map(|target| match self.owner(target) {
                Owner::Wayland(types) => Some((target, types.clone())),
                Owner::Nobody | Owner::X11 => None,
            })
            .collect()
    }
}

/// Whether an X client of the window manager `requester` may read a Wayland
/// selection, given the window manager of the X11 window holding the
/// keyboard, if one does.
///
/// The same rule Wayland clients live by: only the focused client gets the
/// selection.
pub(crate) fn x11_may_read<I: PartialEq>(focused: Option<I>, requester: I) -> bool {
    focused.is_some_and(|focused| focused == requester)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIP: SelectionTarget = SelectionTarget::Clipboard;
    const PRIMARY: SelectionTarget = SelectionTarget::Primary;

    fn text() -> Vec<String> {
        vec!["text/plain;charset=utf-8".into(), "text/plain".into()]
    }

    #[test]
    fn a_wayland_selection_is_offered_to_x11() {
        let mut bridge = Bridge::default();
        assert_eq!(
            bridge.wayland_selection(CLIP, Some(text())),
            Action::OfferToX11(Some(text()))
        );
        assert_eq!(bridge.owner(CLIP), &Owner::Wayland(text()));
        assert_eq!(bridge.owner(PRIMARY), &Owner::Nobody);
    }

    #[test]
    fn a_cleared_or_empty_wayland_selection_withdraws_the_x11_claim() {
        let mut bridge = Bridge::default();
        bridge.wayland_selection(PRIMARY, Some(text()));
        assert_eq!(
            bridge.wayland_selection(PRIMARY, None),
            Action::OfferToX11(None)
        );
        assert_eq!(bridge.owner(PRIMARY), &Owner::Nobody);

        bridge.wayland_selection(PRIMARY, Some(text()));
        assert_eq!(
            bridge.wayland_selection(PRIMARY, Some(Vec::new())),
            Action::OfferToX11(None)
        );
        assert_eq!(bridge.owner(PRIMARY), &Owner::Nobody);
    }

    #[test]
    fn an_x11_selection_becomes_the_seats() {
        let mut bridge = Bridge::default();
        bridge.wayland_selection(CLIP, Some(vec!["image/png".into()]));
        assert_eq!(
            bridge.x11_selection(CLIP, text()),
            Action::SetWayland(text())
        );
        assert_eq!(bridge.owner(CLIP), &Owner::X11);
    }

    #[test]
    fn an_x11_selection_with_no_mime_types_clears_the_seats() {
        let mut bridge = Bridge::default();
        bridge.wayland_selection(PRIMARY, Some(text()));
        assert_eq!(
            bridge.x11_selection(PRIMARY, Vec::new()),
            Action::ClearWayland
        );
        assert_eq!(bridge.owner(PRIMARY), &Owner::Nobody);
        assert_eq!(bridge.x11_selection(PRIMARY, Vec::new()), Action::Nothing);
    }

    #[test]
    fn x11_letting_go_clears_only_its_own_selection() {
        let mut bridge = Bridge::default();
        bridge.x11_selection(PRIMARY, text());
        assert_eq!(bridge.x11_cleared(PRIMARY), Action::ClearWayland);
        assert_eq!(bridge.owner(PRIMARY), &Owner::Nobody);

        // A Wayland client took the clipboard since; X letting go of a
        // selection it no longer owns must not take that away.
        bridge.x11_selection(CLIP, text());
        bridge.wayland_selection(CLIP, Some(text()));
        assert_eq!(bridge.x11_cleared(CLIP), Action::Nothing);
        assert_eq!(bridge.owner(CLIP), &Owner::Wayland(text()));
    }

    #[test]
    fn the_targets_are_independent() {
        let mut bridge = Bridge::default();
        bridge.x11_selection(PRIMARY, text());
        bridge.wayland_selection(CLIP, Some(text()));
        assert_eq!(bridge.owner(PRIMARY), &Owner::X11);
        assert_eq!(bridge.owner(CLIP), &Owner::Wayland(text()));
    }

    #[test]
    fn losing_xwayland_drops_what_stood_in_for_x11() {
        let mut bridge = Bridge::default();
        bridge.x11_selection(PRIMARY, text());
        bridge.wayland_selection(CLIP, Some(text()));
        assert_eq!(bridge.x11_gone(), vec![PRIMARY]);
        assert_eq!(bridge.owner(PRIMARY), &Owner::Nobody);
        assert_eq!(bridge.owner(CLIP), &Owner::Wayland(text()));
    }

    #[test]
    fn a_new_xwayland_is_told_the_wayland_selections() {
        let mut bridge = Bridge::default();
        bridge.wayland_selection(CLIP, Some(text()));
        bridge.x11_selection(PRIMARY, text());
        assert_eq!(bridge.replay(), vec![(CLIP, text())]);
    }

    #[test]
    fn only_the_focused_x11_window_manager_reads_wayland_selections() {
        assert!(x11_may_read(Some(1), 1));
        assert!(!x11_may_read(Some(2), 1));
        assert!(!x11_may_read(None, 1));
    }
}
