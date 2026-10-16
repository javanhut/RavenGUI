//! `wp_presentation`: telling a client when its frame reached the screen.
//!
//! A surface asks for feedback on a commit. When a frame that shows the
//! surface is queued, its feedback is taken into an
//! [`OutputPresentationFeedback`] for that screen; when the flip lands it is
//! answered `presented` with the flip's time, the screen's refresh interval
//! and how the time was measured. A surface the frame did not show keeps its
//! feedback for a later frame, and smithay answers it `discarded` when the
//! client commits over it or destroys the surface. A frame that never
//! reaches the screen drops its feedback, which is also `discarded`.
//!
//! The udev backend answers from the page flip's vblank event. The winit
//! backend has no flip to see, so it answers after the buffer swap, with the
//! time it is then.

use std::collections::HashMap;
use std::time::Duration;

use smithay::{
    backend::{
        drm::{DrmEventMetadata, DrmEventTime},
        renderer::element::{Element, Id, RenderElementPresentationState, RenderElementStates},
    },
    desktop::utils::{OutputPresentationFeedback, take_presentation_feedback_surface_tree},
    output::Output,
    reexports::{
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Clock, Monotonic, Physical, Rectangle, Scale, Size},
    wayland::presentation::Refresh,
};

/// The clock every timestamp is on, as `wp_presentation.clock_id` names it.
pub(crate) const CLOCK_ID: u32 = 1;

/// The elements a frame put on screen, each with the flags its surfaces'
/// feedback carries.
pub(crate) type Shown = HashMap<Id, Kind>;

/// What a frame showed, from the states the DRM compositor reports: every
/// element it did not skip, marked zero-copy when it went straight to a plane.
pub(crate) fn shown_from_states(states: &RenderElementStates) -> Shown {
    states
        .states
        .iter()
        .filter_map(|(id, state)| match state.presentation_state {
            RenderElementPresentationState::Skipped => None,
            RenderElementPresentationState::ZeroCopy => Some((id.clone(), Kind::ZeroCopy)),
            RenderElementPresentationState::Rendering { .. } => Some((id.clone(), Kind::empty())),
        })
        .collect()
}

/// What a frame showed when everything in it is drawn, as on a frame drawn
/// whole into a buffer: every element whose geometry falls on the frame.
pub(crate) fn shown_from_elements<'a, E>(
    elements: impl IntoIterator<Item = &'a E>,
    scale: Scale<f64>,
    frame: Size<i32, Physical>,
) -> Shown
where
    E: Element + 'a,
{
    let frame = Rectangle::from_size(frame);
    elements
        .into_iter()
        .filter(|element| element.geometry(scale).overlaps(frame))
        .map(|element| (element.id().clone(), Kind::empty()))
        .collect()
}

/// Take the feedback of every surface in these trees that the frame showed.
pub(crate) fn collect(
    output: &Output,
    surfaces: impl IntoIterator<Item = WlSurface>,
    shown: &Shown,
) -> OutputPresentationFeedback {
    let mut feedback = OutputPresentationFeedback::new(output);
    for surface in surfaces {
        take_presentation_feedback_surface_tree(
            &surface,
            &mut feedback,
            |surface, _| {
                shown
                    .contains_key(&Id::from(surface))
                    .then(|| output.clone())
            },
            |surface, _| {
                shown
                    .get(&Id::from(surface))
                    .copied()
                    .unwrap_or_else(Kind::empty)
            },
        );
    }
    feedback
}

/// The flip's time from a vblank event, when it is on [`CLOCK_ID`].
pub(crate) fn flip_time(meta: Option<&DrmEventMetadata>) -> Option<Duration> {
    match meta?.time {
        DrmEventTime::Monotonic(time) => Some(time),
        DrmEventTime::Realtime(_) => None,
    }
}

/// The time to report and how it was measured. A flip time from the kernel
/// is the hardware's own; without one the frame is reported at `now`.
pub(crate) fn timing(flip: Option<Duration>, now: Duration) -> (Duration, Kind) {
    match flip {
        Some(time) => (time, Kind::Vsync | Kind::HwClock | Kind::HwCompletion),
        None => (now, Kind::Vsync),
    }
}

/// The refresh interval of a mode refreshing at `mhz` millihertz.
pub(crate) fn refresh(mhz: i32) -> Refresh {
    match u64::try_from(mhz) {
        Ok(mhz) if mhz > 0 => Refresh::Fixed(Duration::from_nanos(1_000_000_000_000 / mhz)),
        _ => Refresh::Unknown,
    }
}

/// The current time on [`CLOCK_ID`].
pub(crate) fn now() -> Duration {
    Clock::<Monotonic>::new().now().into()
}

/// Answer a frame's feedback.
pub(crate) fn presented(
    feedback: &mut OutputPresentationFeedback,
    time: Duration,
    refresh: Refresh,
    seq: u64,
    flags: Kind,
) {
    feedback.presented::<Duration, Monotonic>(time, refresh, seq, flags);
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::{
        element::{RenderElementState, solid::SolidColorRenderElement},
        utils::CommitCounter,
    };
    use smithay::utils::ClockSource;

    fn state(presentation_state: RenderElementPresentationState) -> RenderElementState {
        RenderElementState {
            visible_area: 100,
            presentation_state,
        }
    }

    #[test]
    fn the_clock_is_monotonic() {
        assert_eq!(CLOCK_ID, Monotonic::ID as u32);
    }

    #[test]
    fn a_frame_shows_what_it_did_not_skip() {
        let rendered = Id::new();
        let scanned_out = Id::new();
        let skipped = Id::new();
        let states = RenderElementStates {
            states: HashMap::from([
                (
                    rendered.clone(),
                    state(RenderElementPresentationState::Rendering { reason: None }),
                ),
                (
                    scanned_out.clone(),
                    state(RenderElementPresentationState::ZeroCopy),
                ),
                (
                    skipped.clone(),
                    RenderElementState {
                        visible_area: 0,
                        presentation_state: RenderElementPresentationState::Skipped,
                    },
                ),
            ]),
        };
        let shown = shown_from_states(&states);
        assert_eq!(shown.get(&rendered), Some(&Kind::empty()));
        assert_eq!(shown.get(&scanned_out), Some(&Kind::ZeroCopy));
        assert_eq!(shown.get(&skipped), None);
    }

    #[test]
    fn a_whole_frame_shows_what_falls_on_it() {
        let solid = |id: &Id, x: i32| {
            SolidColorRenderElement::new(
                id.clone(),
                Rectangle::new((x, 0).into(), (50, 50).into()),
                CommitCounter::default(),
                [1.0, 1.0, 1.0, 1.0],
                smithay::backend::renderer::element::Kind::Unspecified,
            )
        };
        let on = Id::new();
        let straddling = Id::new();
        let off = Id::new();
        let elements = [solid(&on, 0), solid(&straddling, 90), solid(&off, 200)];
        let shown = shown_from_elements(&elements, Scale::from(1.0), (100, 100).into());
        assert_eq!(shown.get(&on), Some(&Kind::empty()));
        assert_eq!(shown.get(&straddling), Some(&Kind::empty()));
        assert_eq!(shown.get(&off), None);
    }

    #[test]
    fn a_kernel_flip_time_is_the_hardwares() {
        let flip = Duration::from_millis(1000);
        let (time, flags) = timing(Some(flip), Duration::from_millis(1005));
        assert_eq!(time, flip);
        assert_eq!(flags, Kind::Vsync | Kind::HwClock | Kind::HwCompletion);
    }

    #[test]
    fn without_a_flip_time_the_frame_is_reported_now() {
        let now = Duration::from_millis(1005);
        let (time, flags) = timing(None, now);
        assert_eq!(time, now);
        assert_eq!(flags, Kind::Vsync);
        assert!(!flags.contains(Kind::HwClock));
    }

    #[test]
    fn only_a_monotonic_flip_time_is_used() {
        let monotonic = DrmEventMetadata {
            time: DrmEventTime::Monotonic(Duration::from_secs(7)),
            sequence: 3,
        };
        let realtime = DrmEventMetadata {
            time: DrmEventTime::Realtime(std::time::SystemTime::now()),
            sequence: 3,
        };
        assert_eq!(flip_time(Some(&monotonic)), Some(Duration::from_secs(7)));
        assert_eq!(flip_time(Some(&realtime)), None);
        assert_eq!(flip_time(None), None);
    }

    #[test]
    fn the_refresh_interval_comes_from_the_mode() {
        assert_eq!(
            refresh(60_000),
            Refresh::Fixed(Duration::from_nanos(16_666_666))
        );
        assert_eq!(
            refresh(144_000),
            Refresh::Fixed(Duration::from_nanos(6_944_444))
        );
        assert_eq!(refresh(0), Refresh::Unknown);
        assert_eq!(refresh(-1), Refresh::Unknown);
    }
}
