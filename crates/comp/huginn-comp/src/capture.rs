//! Screen capture for clients: `raven_capture_v1` and
//! `raven_region_selection_v1`.
//!
//! # Why this exists after all
//!
//! Huginn held out without a client capture protocol for a long time — see
//! [`crate::screenshot`] and [`crate::record`], which do the job inside the
//! compositor, bound to keys. Raven Camera changed that: a recorder with a
//! live preview, a choice of screen, window or rectangle, and a file format
//! the compositor has no business knowing about. So the privileged shell
//! protocol grew a capture interface, shaped like the rest of Huginn's
//! capture: the compositor renders the scene offscreen, exactly as a
//! recording does, and copies the pixels into memory the client handed over.
//!
//! The global is not yet limited to privileged clients (see
//! `docs/protocols.md`), which since this interface means any client on the
//! session can read the screen. The recording dot is the one guard there is:
//! a screen being captured says so.
//!
//! # How a frame is made
//!
//! The client sends `frame` with a `wl_shm` buffer. Nothing happens straight
//! away: a timer owned by the backend — the same shape as the recording's —
//! ticks [`tick`] at the captured screen's refresh rate while there is
//! anything to do. On a tick a pending frame whose source has changed since
//! the last one (or that is the first) is rendered offscreen, pointer and
//! click rings included if asked for, recording dot and notifications left
//! out, and its read-back queued. The *next* tick maps the read-back — by
//! then long since finished on the GPU, so the frame loop never waits on it —
//! converts it to the buffer's byte order, copies it in, and answers `ready`.
//!
//! "Changed" is learnt, not computed: a screen that presented a new frame
//! marks the captures of it, and a commit to a captured window marks that
//! capture, since a window can change while something covers it and the
//! screen shows nothing new.
//!
//! Each capture keeps its texture and staging buffer between frames and
//! reallocates only on a size change: the machine this has to run well on is
//! a Celeron, and eight megabytes allocated and freed thirty times a second is
//! work it has better uses for. A capture with no pending frame costs nothing
//! but its memory.
//!
//! # What the client never gets
//!
//! Anything while the session is locked — a pending frame waits for the
//! unlock, and a read-back from before the lock is thrown away rather than
//! delivered after it. The lock screen is never rendered for a capture: the
//! capture is not even assembled while locked.
//!
//! Everything from [`to_bgra`] down is plain arithmetic with no renderer or
//! client in it, so it is unit-tested without a GPU.

use std::time::{Duration, Instant};

use raven_protocol::server::{
    raven_capture_v1::{self, Options, RavenCaptureV1},
    raven_region_selection_v1::{self, RavenRegionSelectionV1},
};
use smithay::{
    backend::renderer::{
        Color32F, ExportMem,
        gles::{GlesMapping, GlesRenderer, GlesTexture},
    },
    reexports::wayland_server::{
        Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum,
        backend::ClientId,
        protocol::{wl_buffer::WlBuffer, wl_shm, wl_surface::WlSurface},
    },
    utils::{Clock, Logical, Monotonic, Physical, Point, Rectangle, Size},
    wayland::shm::{BufferData, with_buffer_contents},
};

use huginn_core::geometry::Rect;
use huginn_core::window::WindowId;

use crate::canvas::{Canvas, Panel};
use crate::pointer::Cursor;
use crate::state::Huginn;

/// How long a click ring takes to fade out.
const RING_LIFE: Duration = Duration::from_millis(450);
/// The ring's radius as it appears and as it vanishes, logical pixels.
const RING_FROM: f64 = 10.0;
const RING_TO: f64 = 26.0;
/// The ring's stroke, logical pixels, at its largest.
const RING_STROKE: f32 = 3.0;
/// The density the ring is composed at. Drawn scaled to its size every frame
/// anyway, so one bitmap at 2x serves a 1x and a 2x screen alike.
const RING_DENSITY: u32 = 2;

/// How long after its last delivered frame a capture keeps the recording dot
/// up. Long enough that a recorder at a low frame rate, or a still desktop
/// that sends nothing new, does not make the dot flicker.
const DOT_HOLD: Duration = Duration::from_secs(1);

/// The refresh assumed for a screen that does not report one.
const DEFAULT_REFRESH_MHZ: i32 = 60_000;

/// What a capture is of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// A screen, by connector name.
    Output(String),
    /// A window, by `ext_foreign_toplevel_handle_v1` identifier.
    Window(String),
    /// A rectangle of a screen, in logical pixels relative to its corner, as
    /// the client asked for it: clipped to the screen whenever it is
    /// resolved, so a screen that changes mode clips it afresh.
    Region { output: String, rect: Rect },
}

/// A source, as it stands this moment: what to draw and at what size.
#[derive(Debug, Clone)]
pub(crate) struct View {
    /// The area drawn, in the desktop's global logical pixels.
    rect: Rect,
    /// Fractional render scale of the screen it is on.
    scale: f64,
    /// That screen's advertised integer scale: which cursor bitmap to use.
    density: u32,
    /// The size in physical pixels a frame is, and so a buffer must be.
    size: Size<i32, Physical>,
    /// The connector name of the screen it is on.
    output: String,
    /// That screen's refresh period, the fastest a capture of it is served.
    period: Duration,
    /// For a window capture: which window, and its surface.
    window: Option<(WindowId, WlSurface)>,
    /// Whether it is on screen to be drawn. Only a window is ever not.
    shown: bool,
}

/// One client capture.
#[derive(Debug)]
struct Capture {
    resource: RavenCaptureV1,
    source: Source,
    options: Options,
    /// The last `buffer_size` sent. Always sent at creation, so `None` only
    /// for a capture that was stopped from the start.
    size: Option<Size<i32, Physical>>,
    /// The buffer of the frame request not yet answered.
    pending: Option<WlBuffer>,
    /// A frame drawn for `pending`, still on its way back from the GPU.
    in_flight: Option<InFlight>,
    /// The source has changed since the last frame was drawn.
    damaged: bool,
    /// No frame delivered yet: the first request is answered straight away,
    /// damage or no.
    first: bool,
    /// `stopped` has been sent; nothing more will be.
    stopped: bool,
    /// Drawn into on every frame and created again only when the size changes.
    texture: Option<(GlesTexture, Size<i32, Physical>)>,
    /// The frame in the client's byte order, before it is copied in. Kept for
    /// the same reason the texture is.
    staging: Vec<u8>,
    /// When the last frame was drawn, for the refresh cap.
    last_render: Option<Instant>,
    /// When the last frame was delivered, for the recording dot.
    delivered: Option<Instant>,
    /// The screen the source was on at the last look.
    output: Option<String>,
    /// For a window capture: its surface at the last look, to match commits
    /// against.
    surface: Option<WlSurface>,
    /// The last frame had click rings in it, so the next must be drawn even
    /// if nothing else changed: the rings have moved on, or gone.
    rings_drawn: bool,
}

/// A read-back queued on one tick, to be collected on the next.
#[derive(Debug)]
struct InFlight {
    mapping: GlesMapping,
    /// CLOCK_MONOTONIC when the frame was drawn.
    time: Duration,
    size: Size<i32, Physical>,
    /// Not mapped before this. A tick can come early — a kick from a frame
    /// request or a screen that just presented — and mapping a read-back the
    /// GPU has not finished would stall the frame loop until it has.
    ready_at: Instant,
}

/// Every client capture, and what they share.
#[derive(Debug, Default)]
pub(crate) struct Captures {
    list: Vec<Capture>,
    /// Pointer presses of the last [`RING_LIFE`]: where, in desktop logical
    /// pixels, and when.
    clicks: Vec<(Point<f64, Logical>, Instant)>,
    /// The click ring's bitmap, composed the first time one is drawn.
    ring: Option<Panel>,
    /// Something wants a tick sooner than the timer would give one: a frame
    /// was asked for, or a source with a frame pending changed.
    kick: bool,
}

impl Captures {
    /// Whether any capture is of a window, so a commit is worth matching.
    pub(crate) fn has_windows(&self) -> bool {
        self.list
            .iter()
            .any(|c| !c.stopped && matches!(c.source, Source::Window(_)))
    }

    /// `surface` — a root surface — committed: a capture of its window has
    /// something new.
    pub(crate) fn note_commit(&mut self, surface: &WlSurface) {
        for capture in &mut self.list {
            if capture.surface.as_ref() == Some(surface) {
                capture.damaged = true;
                self.kick |= capture.waiting();
            }
        }
    }

    /// Screen `output` presented a new frame: everything captured from it has
    /// something new — a window capture too, since its bar, its popups or
    /// the pointer over it may be what changed.
    pub(crate) fn note_damage(&mut self, output: &str) {
        for capture in &mut self.list {
            if capture.output.as_deref() == Some(output) {
                capture.damaged = true;
                self.kick |= capture.waiting();
            }
        }
    }

    /// Damage every capture: for a backend that cannot say which screen drew.
    pub(crate) fn note_damage_all(&mut self) {
        for capture in &mut self.list {
            capture.damaged = true;
            self.kick |= capture.waiting();
        }
    }

    /// The pointer was pressed at `at`. Remembered only if a capture wants
    /// click rings; otherwise a press costs nothing.
    pub(crate) fn note_click(&mut self, at: Point<f64, Logical>) {
        if !self
            .list
            .iter()
            .any(|c| !c.stopped && c.options.contains(Options::Clicks))
        {
            return;
        }
        let now = Instant::now();
        self.clicks
            .retain(|(_, when)| now.saturating_duration_since(*when) < RING_LIFE);
        self.clicks.push((at, now));
        self.kick = true;
    }

    /// Take the request for an early tick. The backend re-arms its timer to
    /// fire at once when this is true.
    pub(crate) fn take_kick(&mut self) -> bool {
        std::mem::take(&mut self.kick)
    }

    /// Whether the backend's timer has anything to do: a frame waiting to be
    /// drawn or collected. Without one it drops itself, and a capture that
    /// is not being asked for frames costs no wakeups at all.
    pub(crate) fn wants_ticks(&self) -> bool {
        self.list
            .iter()
            .any(|c| !c.stopped && (c.pending.is_some() || c.in_flight.is_some()))
    }

    /// Screens to show the recording dot on: every one a capture has
    /// delivered a frame of within [`DOT_HOLD`]. Sorted and deduplicated, so
    /// an unchanged set compares equal.
    fn dot_outputs(&self, now: Instant) -> Vec<String> {
        let mut out: Vec<String> = self
            .list
            .iter()
            .filter(|c| {
                c.delivered
                    .is_some_and(|at| now.saturating_duration_since(at) < DOT_HOLD)
            })
            .filter_map(|c| c.output.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn find(&mut self, resource: &RavenCaptureV1) -> Option<&mut Capture> {
        self.list.iter_mut().find(|c| &c.resource == resource)
    }
}

impl Capture {
    /// A frame is pending and nothing has been drawn for it yet: a change to
    /// the source is worth an early tick.
    fn waiting(&self) -> bool {
        !self.stopped && self.pending.is_some() && self.in_flight.is_none()
    }

    /// Answer the pending frame, if there is one, with `failed`.
    fn fail_pending(&mut self) {
        self.in_flight = None;
        if self.pending.take().is_some() {
            self.resource.failed();
        }
    }

    /// The source is gone: `failed` for a pending frame, then `stopped`, and
    /// the memory given back.
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.fail_pending();
        self.resource.stopped();
        self.stopped = true;
        self.texture = None;
        self.staging = Vec::new();
        self.surface = None;
    }

    /// Catch up with where the source is now. A new size is announced, and
    /// fails the pending frame, whose buffer is now the wrong size.
    fn follow(&mut self, view: &View) {
        if self.output.as_deref() != Some(view.output.as_str()) {
            self.output = Some(view.output.clone());
        }
        let surface = view.window.as_ref().map(|(_, surface)| surface);
        if self.surface.as_ref() != surface {
            self.surface = surface.cloned();
        }
        // A window with nothing drawn yet has no size worth announcing; it
        // keeps the last one until it does.
        if view.size.w <= 0 || view.size.h <= 0 || self.size == Some(view.size) {
            return;
        }
        self.size = Some(view.size);
        self.resource
            .buffer_size(view.size.w as u32, view.size.h as u32);
        self.fail_pending();
        self.damaged = true;
    }

    /// Collect the frame drawn on the last tick into the client's buffer and
    /// answer `ready`, or `failed` if it cannot be.
    fn deliver(&mut self, renderer: &mut GlesRenderer, flight: InFlight, now: Instant) {
        let Some(buffer) = self.pending.take() else {
            return;
        };
        let result = (|| -> anyhow::Result<()> {
            let pixels = renderer
                .map_texture(&flight.mapping)
                .map_err(|e| anyhow::anyhow!("mapping a captured frame: {e}"))?;
            let row = flight.size.w as usize * 4;
            let rows = flight.size.h as usize;
            let frame = pixels
                .get(..row * rows)
                .ok_or_else(|| anyhow::anyhow!("the read-back is short"))?;
            to_bgra(frame, &mut self.staging);
            huginn_egl::write_shm_rows(&buffer, &self.staging, row, row, rows)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let (hi, lo, nsec) = split_time(flight.time);
                self.resource.ready(hi, lo, nsec);
                self.delivered = Some(now);
                self.first = false;
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "capture frame failed");
                self.resource.failed();
            }
        }
    }

    /// Draw the next frame of `view` and queue its read-back.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &Huginn,
        view: &View,
        cursor: Option<&Cursor>,
        rings: &[(Point<f64, Logical>, f64, f32)],
        ring: Option<&Panel>,
        now: Instant,
    ) -> anyhow::Result<()> {
        if self.texture.as_ref().map(|(_, size)| *size) != Some(view.size) {
            self.texture = None;
            let texture = crate::screenshot::offscreen_texture(renderer, view.size)?;
            self.texture = Some((texture, view.size));
        }
        let Some((texture, _)) = self.texture.as_mut() else {
            unreachable!("created just above");
        };
        let elements = crate::render::client_capture_elements(
            renderer,
            state,
            cursor,
            self.options.contains(Options::Cursor),
            view.window.as_ref().map(|(id, _)| *id),
            view.rect,
            view.scale,
            rings,
            ring.map(Panel::buffer),
        );
        // A window alone is drawn over nothing, so its corners are its own.
        let clear = if view.window.is_some() {
            Color32F::TRANSPARENT
        } else {
            DESKTOP_CLEAR
        };
        let time = Duration::from(Clock::<Monotonic>::new().now());
        let mapping = crate::screenshot::draw_offscreen_over(
            renderer, texture, &elements, view.size, view.scale, false, clear,
        )?;
        self.in_flight = Some(InFlight {
            mapping,
            time,
            size: view.size,
            // Half a refresh gives the GPU ample time on the weakest machine
            // this runs on, and the client its frame soon.
            ready_at: now + view.period / 2,
        });
        Ok(())
    }
}

/// Behind the desktop, as on screen. See `crate::screenshot`.
const DESKTOP_CLEAR: Color32F = Color32F::new(0.06, 0.06, 0.09, 1.0);

impl Huginn {
    /// Where `source` is now, or `None` if it is gone: a screen unplugged, a
    /// window closed, a rectangle clipped to nothing.
    fn resolve_capture(&self, source: &Source) -> Option<View> {
        let screen = |name: &str| {
            self.outputs()
                .iter()
                .find(|info| info.name == name && info.output.is_some())
        };
        match source {
            Source::Output(name) => {
                let info = screen(name)?;
                let mode = info.output.as_ref()?.current_mode()?;
                Some(View {
                    rect: info.rect,
                    scale: info.scale.fractional(),
                    density: info.scale.advertised,
                    size: info.frame_size()?,
                    output: info.name.clone(),
                    period: frame_period(mode.refresh),
                    window: None,
                    shown: true,
                })
            }
            Source::Region { output, rect } => {
                let info = screen(output)?;
                let mode = info.output.as_ref()?.current_mode()?;
                let scale = info.scale.fractional();
                let (local, physical) = clip_region(*rect, info.rect, scale, info.frame_size()?)?;
                Some(View {
                    rect: Rect::from_xywh(
                        info.rect.x() + local.x(),
                        info.rect.y() + local.y(),
                        local.w(),
                        local.h(),
                    ),
                    scale,
                    density: info.scale.advertised,
                    size: physical.size,
                    output: info.name.clone(),
                    period: frame_period(mode.refresh),
                    window: None,
                    shown: true,
                })
            }
            Source::Window(identifier) => {
                let id = self.window_by_identifier(identifier)?;
                let (rect, shown, surface) = self.window_capture_frame(id)?;
                let info = self.outputs().get(self.output_of_rect(rect))?;
                let scale = info.scale.fractional();
                let refresh = info
                    .output
                    .as_ref()
                    .and_then(|output| output.current_mode())
                    .map_or(DEFAULT_REFRESH_MHZ, |mode| mode.refresh);
                let size = physical_size(rect, scale);
                Some(View {
                    rect,
                    scale,
                    density: info.scale.advertised,
                    size,
                    output: info.name.clone(),
                    period: frame_period(refresh),
                    window: Some((id, surface)),
                    shown: shown && size.w > 0 && size.h > 0,
                })
            }
        }
    }

    /// A client asked for a capture of `source`. The first event goes out
    /// now: `buffer_size`, or `stopped` for a source that does not exist.
    pub(crate) fn create_capture(&mut self, resource: RavenCaptureV1, source: Source, options: Options) {
        let mut capture = Capture {
            resource,
            source,
            options,
            size: None,
            pending: None,
            in_flight: None,
            damaged: true,
            first: true,
            stopped: false,
            texture: None,
            staging: Vec::new(),
            last_render: None,
            delivered: None,
            output: None,
            surface: None,
            rings_drawn: false,
        };
        match self.resolve_capture(&capture.source) {
            Some(view) => {
                capture.follow(&view);
                if capture.size.is_none() {
                    // A window that has drawn nothing measurable yet. The
                    // protocol promises buffer_size first, so it gets one —
                    // the honest one, empty — and the real one follows.
                    capture.size = Some(Size::from((0, 0)));
                    capture.resource.buffer_size(0, 0);
                }
                tracing::debug!(source = ?capture.source, size = ?capture.size, "capture created");
            }
            None => {
                tracing::debug!(source = ?capture.source, "capture of nothing; stopped");
                capture.stop();
            }
        }
        self.captures.list.push(capture);
    }

    /// A client asked for the next frame of a capture, into `buffer`.
    fn capture_frame(&mut self, resource: &RavenCaptureV1, buffer: WlBuffer) {
        // Resolved before the capture is borrowed: resolving reads the rest
        // of the compositor.
        let Some(source) = self.captures.find(resource).map(|c| c.source.clone()) else {
            return;
        };
        let view = self.resolve_capture(&source);
        let Some(capture) = self.captures.find(resource) else {
            return;
        };
        if capture.stopped {
            // The protocol has nothing left to say after `stopped`, and a
            // client that has not heard it yet will.
            return;
        }
        if capture.pending.is_some() {
            resource.post_error(
                raven_capture_v1::Error::AlreadyPending,
                "frame requested while another is pending",
            );
            return;
        }
        let Some(view) = view else {
            capture.pending = Some(buffer);
            capture.stop();
            return;
        };
        capture.pending = Some(buffer.clone());
        // A source that changed size since the client last heard is told so
        // now, and this frame fails with it: its buffer was sized for the old.
        capture.follow(&view);
        if capture.pending.is_none() {
            return;
        }
        let expected = capture.size.unwrap_or_default();
        let checked = with_buffer_contents(&buffer, |_, len, data| check_buffer(&data, len, expected));
        match checked {
            Ok(Ok(())) => self.captures.kick = true,
            Ok(Err(why)) => {
                tracing::debug!(?why, "capture buffer refused");
                capture.fail_pending();
            }
            Err(e) => {
                tracing::debug!(error = %e, "capture buffer is not wl_shm");
                capture.fail_pending();
            }
        }
    }

    /// Look at every capture's source again, outside a tick: a screen
    /// unplugged or re-moded, a window closed. `stopped` and `buffer_size`
    /// go out at once rather than when the client next asks for a frame.
    pub(crate) fn refresh_captures(&mut self) {
        if self.captures.list.is_empty() {
            return;
        }
        let views: Vec<Option<View>> = self
            .captures
            .list
            .iter()
            .map(|c| {
                if c.stopped {
                    None
                } else {
                    self.resolve_capture(&c.source)
                }
            })
            .collect();
        for (capture, view) in self.captures.list.iter_mut().zip(views) {
            match view {
                _ if capture.stopped => {}
                Some(view) => capture.follow(&view),
                None => capture.stop(),
            }
        }
    }

    /// The pointer was pressed: a ring for captures that draw clicks.
    pub(crate) fn note_capture_click(&mut self) {
        let at = self.pointer_location;
        self.captures.note_click(at);
    }
}

/// One tick of every client capture: collect last tick's read-backs, draw
/// what is due, and keep the recording dots honest.
///
/// `cursor` finds the theme cursor for a density, as the backend holds them.
/// Returns how long until the next tick is wanted, or `None` when there is
/// nothing left to do and the timer can go.
pub(crate) fn tick<'c>(
    renderer: &mut GlesRenderer,
    state: &mut Huginn,
    cursor: &dyn Fn(u32) -> Option<&'c Cursor>,
) -> Option<Duration> {
    let now = Instant::now();
    let locked = state.is_locked();
    // Taken out for the tick, so each capture can be changed while the scene
    // it draws is read from the rest of the state. Nothing can add one in the
    // meantime: requests are dispatched on this same thread.
    let mut captures = std::mem::take(&mut state.captures);
    captures
        .clicks
        .retain(|(_, when)| now.saturating_duration_since(*when) < RING_LIFE);
    let rings: Vec<(Point<f64, Logical>, f64, f32)> = captures
        .clicks
        .iter()
        .filter_map(|(at, when)| {
            let (radius, alpha) = ring_at(now.saturating_duration_since(*when))?;
            Some((*at, radius, alpha))
        })
        .collect();
    if !rings.is_empty() && captures.ring.is_none() {
        captures.ring = Some(compose_ring());
    }
    let mut next: Option<Duration> = None;
    let mut soonest = |wait: Duration| next = Some(next.map_or(wait, |n: Duration| n.min(wait)));

    let Captures { list, ring, .. } = &mut captures;
    for capture in list.iter_mut().filter(|c| !c.stopped) {
        if let Some(flight) = capture.in_flight.take() {
            if locked {
                // Drawn before the lock, and not to be delivered after it:
                // drawn again once the session is back.
                capture.damaged = true;
            } else if now < flight.ready_at {
                // An early tick: leave it to the GPU a little longer.
                soonest(flight.ready_at - now);
                capture.in_flight = Some(flight);
                continue;
            } else {
                capture.deliver(renderer, flight, now);
            }
        }
        let Some(view) = state.resolve_capture(&capture.source) else {
            capture.stop();
            continue;
        };
        capture.follow(&view);
        if capture.pending.is_none() {
            continue;
        }
        // Still waiting for something: the next look is a refresh away.
        soonest(view.period);
        if locked || !view.shown {
            continue;
        }
        let with_rings = capture.options.contains(Options::Clicks) && !rings.is_empty();
        if !wants_frame(capture.first, capture.damaged, with_rings, capture.rings_drawn) {
            continue;
        }
        if let Some(wait) = until_due(capture.last_render, now, view.period) {
            soonest(wait);
            continue;
        }
        let drawn: &[(Point<f64, Logical>, f64, f32)] = if with_rings { &rings } else { &[] };
        render_one(renderer, state, capture, &view, cursor, drawn, ring.as_ref(), now);
        // Collected on the tick after, once the GPU has had its time.
        soonest(view.period / 2);
    }

    // Stopped captures stay until the client destroys them — the object is
    // its to destroy — but hold no memory in the meantime; see `stop`. The
    // kick was for this tick, which has now happened.
    let dots = captures.dot_outputs(now);
    captures.kick = false;
    state.captures = captures;
    state.set_capture_dots(&dots);
    if !dots.is_empty() {
        // Come back to take the dot down.
        soonest(DOT_HOLD / 4);
    }
    next
}

/// Draw one capture's frame, answering `failed` if it cannot be drawn.
#[allow(clippy::too_many_arguments)]
fn render_one<'c>(
    renderer: &mut GlesRenderer,
    state: &Huginn,
    capture: &mut Capture,
    view: &View,
    cursor: &dyn Fn(u32) -> Option<&'c Cursor>,
    rings: &[(Point<f64, Logical>, f64, f32)],
    ring: Option<&Panel>,
    now: Instant,
) {
    match capture.render(renderer, state, view, cursor(view.density), rings, ring, now) {
        Ok(()) => {
            capture.damaged = false;
            capture.rings_drawn = !rings.is_empty();
            capture.last_render = Some(now);
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "drawing a capture frame");
            capture.fail_pending();
        }
    }
}

/// The ring a click leaves in a capture: an accent circle, composed once at
/// its largest and drawn smaller as it grows in.
fn compose_ring() -> Panel {
    let side = ((RING_TO * 2.0) as usize) * RING_DENSITY as usize;
    let mut canvas = Canvas::new(side, side);
    canvas.stroke_rounded(
        0,
        0,
        side,
        side,
        side as f32 / 2.0,
        RING_STROKE * RING_DENSITY as f32,
        crate::theme::ACCENT,
    );
    Panel::from_canvas(&canvas, RING_DENSITY)
}

impl Dispatch<RavenCaptureV1, ()> for Huginn {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &RavenCaptureV1,
        request: raven_capture_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            raven_capture_v1::Request::Frame { buffer } => state.capture_frame(resource, buffer),
            raven_capture_v1::Request::Destroy => {}
            _ => {}
        }
    }

    /// Destroyed by the client, or with it: a pending frame is abandoned —
    /// its buffer is the client's again, and nobody is left to tell — and the
    /// capture's texture goes with it.
    fn destroyed(state: &mut Self, _client: ClientId, resource: &RavenCaptureV1, _data: &()) {
        state.captures.list.retain(|c| &c.resource != resource);
    }
}

impl Dispatch<RavenRegionSelectionV1, ()> for Huginn {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &RavenRegionSelectionV1,
        request: raven_region_selection_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Destroy is the only request, and `destroyed` does the work.
        let _ = request;
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &RavenRegionSelectionV1, _data: &()) {
        state.region_client_gone(resource);
    }
}

/// The options a request carried. Bits this compositor does not know are
/// dropped rather than refused: a newer client asking for more than this
/// compositor draws gets what it draws.
pub(crate) fn options(options: WEnum<Options>) -> Options {
    match options {
        WEnum::Value(options) => options,
        WEnum::Unknown(bits) => Options::from_bits_truncate(bits),
    }
}

// --- Pure arithmetic from here down -----------------------------------------

/// Convert a read-back — `R, G, B, A` in memory, which is how the offscreen
/// texture comes back (DRM `ABGR8888` on a little-endian host) — into
/// `wl_shm`'s `argb8888`/`xrgb8888`, which in memory on a little-endian host
/// is `B, G, R, A`. Written into `out`, which keeps its allocation from frame
/// to frame; it only grows when the frame does.
fn to_bgra(rgba: &[u8], out: &mut Vec<u8>) {
    out.resize(rgba.len(), 0);
    for (from, to) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
        to[0] = from[2];
        to[1] = from[1];
        to[2] = from[0];
        to[3] = from[3];
    }
}

/// Why a buffer handed to `frame` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    Format,
    Size,
    Stride,
    /// The buffer claims more of its pool than the pool has.
    Pool,
}

/// Whether a `wl_shm` buffer can take a frame of `size`: `argb8888` or
/// `xrgb8888`, exactly that size, four bytes a pixel or more, and inside its
/// pool. `pool` is the pool's length in bytes.
fn check_buffer(data: &BufferData, pool: usize, size: Size<i32, Physical>) -> Result<(), Refused> {
    if !matches!(data.format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        return Err(Refused::Format);
    }
    if data.width != size.w || data.height != size.h || size.w <= 0 || size.h <= 0 {
        return Err(Refused::Size);
    }
    if i64::from(data.stride) < i64::from(data.width) * 4 {
        return Err(Refused::Stride);
    }
    let end = i64::from(data.offset)
        + i64::from(data.stride) * i64::from(data.height - 1)
        + i64::from(data.width) * 4;
    if data.offset < 0 || end > pool as i64 {
        return Err(Refused::Pool);
    }
    Ok(())
}

/// A capture's region, clipped to its screen: the logical rectangle relative
/// to the screen, and the same in the screen's physical pixels, clamped to
/// its mode. `None` when nothing is left.
///
/// Converted edge by edge rather than as origin and size, so two regions
/// that meet on screen meet in pixels too.
fn clip_region(
    rect: Rect,
    screen: Rect,
    scale: f64,
    mode: Size<i32, Physical>,
) -> Option<(Rect, Rectangle<i32, Physical>)> {
    let local = rect.intersection(Rect::from_xywh(0, 0, screen.w(), screen.h()))?;
    let edge = |v: i32, limit: i32| ((f64::from(v) * scale).round() as i32).clamp(0, limit);
    let (x0, y0) = (edge(local.x(), mode.w), edge(local.y(), mode.h));
    let (x1, y1) = (edge(local.right(), mode.w), edge(local.bottom(), mode.h));
    (x1 > x0 && y1 > y0).then(|| {
        (
            local,
            Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()),
        )
    })
}

/// A logical rectangle's size in physical pixels at `scale`.
fn physical_size(rect: Rect, scale: f64) -> Size<i32, Physical> {
    Size::from((
        (f64::from(rect.w()) * scale).round() as i32,
        (f64::from(rect.h()) * scale).round() as i32,
    ))
}

/// A screen's refresh period from its refresh rate in millihertz, which is
/// how modes carry it. Zero or nonsense means 60 Hz; anything faster than
/// 240 Hz is served at 240 — a capture is not a game.
fn frame_period(refresh_mhz: i32) -> Duration {
    let mhz = if refresh_mhz > 0 {
        refresh_mhz
    } else {
        DEFAULT_REFRESH_MHZ
    };
    Duration::from_micros(1_000_000_000 / u64::from(mhz.unsigned_abs())).max(Duration::from_micros(1_000_000 / 240))
}

/// How long until a capture last drawn at `last` may be drawn again, or
/// `None` if it may be now. Capped at the refresh period, with an eighth of
/// it given back: a timer that fires a hair early must not push every other
/// frame to the tick after.
fn until_due(last: Option<Instant>, now: Instant, period: Duration) -> Option<Duration> {
    let spacing = period.saturating_sub(period / 8);
    let since = now.saturating_duration_since(last?);
    (since < spacing).then(|| spacing - since)
}

/// Whether a capture with a frame pending should draw it now: the first frame
/// always, then only when the source has changed — or when click rings are
/// fading through it, or were in the last frame and have to be taken out.
fn wants_frame(first: bool, damaged: bool, rings_live: bool, rings_drawn: bool) -> bool {
    first || damaged || rings_live || rings_drawn
}

/// A click ring `age` after the press: its radius in logical pixels and its
/// opacity, or `None` once it has faded. Grows fast and slows (ease-out),
/// fading linearly, so it reads as a ripple leaving the pointer.
fn ring_at(age: Duration) -> Option<(f64, f32)> {
    if age >= RING_LIFE {
        return None;
    }
    let t = age.as_secs_f64() / RING_LIFE.as_secs_f64();
    let eased = 1.0 - (1.0 - t).powi(3);
    Some((RING_FROM + (RING_TO - RING_FROM) * eased, (1.0 - t) as f32))
}

/// A CLOCK_MONOTONIC time as `ready`'s three words: seconds high and low,
/// and nanoseconds.
fn split_time(time: Duration) -> (u32, u32, u32) {
    let secs = time.as_secs();
    ((secs >> 32) as u32, secs as u32, time.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_becomes_bgra_in_memory() {
        let mut out = Vec::new();
        to_bgra(&[1, 2, 3, 4, 10, 20, 30, 40], &mut out);
        assert_eq!(out, [3, 2, 1, 4, 30, 20, 10, 40]);
        // The staging buffer is reused, not grown, for a frame the same size.
        let capacity = out.capacity();
        to_bgra(&[5, 6, 7, 8, 50, 60, 70, 80], &mut out);
        assert_eq!(out, [7, 6, 5, 8, 70, 60, 50, 80]);
        assert_eq!(out.capacity(), capacity);
    }

    fn shm(format: wl_shm::Format, width: i32, height: i32, stride: i32) -> BufferData {
        BufferData {
            offset: 0,
            width,
            height,
            stride,
            format,
        }
    }

    #[test]
    fn a_buffer_must_match_format_size_stride_and_pool() {
        let size = Size::from((100, 50));
        let pool = 100 * 4 * 50;
        assert_eq!(check_buffer(&shm(wl_shm::Format::Argb8888, 100, 50, 400), pool, size), Ok(()));
        assert_eq!(check_buffer(&shm(wl_shm::Format::Xrgb8888, 100, 50, 400), pool, size), Ok(()));
        assert_eq!(
            check_buffer(&shm(wl_shm::Format::Rgb565, 100, 50, 400), pool, size),
            Err(Refused::Format)
        );
        assert_eq!(
            check_buffer(&shm(wl_shm::Format::Argb8888, 99, 50, 400), pool, size),
            Err(Refused::Size)
        );
        assert_eq!(
            check_buffer(&shm(wl_shm::Format::Argb8888, 100, 50, 399), pool, size),
            Err(Refused::Stride)
        );
        // Padded rows are fine, as long as the pool holds them.
        assert_eq!(
            check_buffer(&shm(wl_shm::Format::Argb8888, 100, 50, 512), pool, size),
            Err(Refused::Pool)
        );
        assert_eq!(
            check_buffer(&shm(wl_shm::Format::Argb8888, 100, 50, 512), 512 * 49 + 400, size),
            Ok(())
        );
        // Nothing fits an empty size, whatever the buffer.
        assert_eq!(
            check_buffer(&shm(wl_shm::Format::Argb8888, 0, 0, 0), 0, Size::from((0, 0))),
            Err(Refused::Size)
        );
    }

    #[test]
    fn a_region_is_clipped_to_its_screen() {
        let screen = Rect::from_xywh(1920, 0, 1280, 800);
        let mode = Size::from((2560, 1600));
        // Hanging off the right edge, at 2x.
        let (local, physical) =
            clip_region(Rect::from_xywh(1200, 100, 200, 100), screen, 2.0, mode).unwrap();
        assert_eq!(local, Rect::from_xywh(1200, 100, 80, 100));
        assert_eq!((physical.loc.x, physical.loc.y), (2400, 200));
        assert_eq!((physical.size.w, physical.size.h), (160, 200));
        // Wholly off the screen: nothing.
        assert!(clip_region(Rect::from_xywh(-500, 0, 100, 100), screen, 2.0, mode).is_none());
        // Empty to begin with: nothing.
        assert!(clip_region(Rect::from_xywh(10, 10, 0, 50), screen, 2.0, mode).is_none());
    }

    #[test]
    fn a_fractional_region_rounds_its_edges_not_its_size() {
        let screen = Rect::from_xywh(0, 0, 1707, 960);
        let mode = Size::from((2560, 1440));
        let (_, a) = clip_region(Rect::from_xywh(0, 0, 101, 10), screen, 1.5, mode).unwrap();
        let (_, b) = clip_region(Rect::from_xywh(101, 0, 101, 10), screen, 1.5, mode).unwrap();
        // Two regions that meet on screen meet in pixels: no gap, no overlap.
        assert_eq!(a.loc.x + a.size.w, b.loc.x);
    }

    #[test]
    fn a_window_is_its_logical_size_at_its_screens_scale() {
        let size = physical_size(Rect::from_xywh(5, 5, 640, 481), 1.5);
        assert_eq!((size.w, size.h), (960, 722));
    }

    #[test]
    fn the_refresh_period_comes_from_millihertz() {
        assert_eq!(frame_period(60_000), Duration::from_micros(16_666));
        assert_eq!(frame_period(0), frame_period(60_000));
        assert_eq!(frame_period(-5), frame_period(60_000));
        assert_eq!(frame_period(1_000_000), Duration::from_micros(4_166));
    }

    #[test]
    fn frames_are_capped_at_the_refresh_with_some_slack() {
        let period = Duration::from_millis(16);
        let now = Instant::now();
        assert_eq!(until_due(None, now, period), None, "never drawn: due");
        let just = now - Duration::from_millis(2);
        assert_eq!(until_due(Some(just), now, period), Some(Duration::from_millis(12)));
        // A tick a millisecond early still counts.
        let early = now - Duration::from_millis(15);
        assert_eq!(until_due(Some(early), now, period), None);
    }

    #[test]
    fn a_still_source_draws_nothing_after_the_first_frame() {
        assert!(wants_frame(true, false, false, false), "the first frame is at once");
        assert!(!wants_frame(false, false, false, false));
        assert!(wants_frame(false, true, false, false));
        assert!(wants_frame(false, false, true, false), "a ring is fading");
        assert!(wants_frame(false, false, false, true), "a ring must be taken out");
    }

    #[test]
    fn a_click_ring_grows_and_fades_then_is_gone() {
        let (r0, a0) = ring_at(Duration::ZERO).unwrap();
        assert_eq!((r0, a0), (RING_FROM, 1.0));
        let (r1, a1) = ring_at(Duration::from_millis(225)).unwrap();
        assert!(r1 > r0 && r1 < RING_TO);
        assert!((a1 - 0.5).abs() < 1e-6);
        // Ease-out: more than half the growth in the first half of the life.
        assert!(r1 - RING_FROM > (RING_TO - RING_FROM) / 2.0);
        let (r2, a2) = ring_at(Duration::from_millis(449)).unwrap();
        assert!(r2 <= RING_TO && a2 < 0.01);
        assert!(ring_at(RING_LIFE).is_none());
        assert!(ring_at(Duration::from_secs(3)).is_none());
    }

    #[test]
    fn monotonic_time_splits_into_three_words() {
        let time = Duration::new((7u64 << 32) + 9, 123);
        assert_eq!(split_time(time), (7, 9, 123));
    }
}
