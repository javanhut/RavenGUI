//! Probe for `xdg_popup` placement.
//!
//! Opens a window, hangs a popup off it, and prints the position the compositor
//! configures the popup at — twice: once where the popup fits and the client's
//! own positioner should be honoured verbatim, and once where it does not and
//! the compositor is required to flip it back on screen.
//!
//! Placement is the half of popup support that a screenshot cannot check. A
//! menu that opens ten pixels off is a bug you find by squinting; a menu that
//! opens under the bottom edge of the monitor is one you find by not seeing it
//! at all. Both show up here as a number that is either right or wrong.
//!
//! ```sh
//! WAYLAND_DISPLAY=huginn-1 cargo run -p muninn --example popup-probe
//! ```
//!
//! Both popups anchor bottom-left and open down and to the right, and offer the
//! compositor `FlipY` as the only adjustment. That leaves exactly two legal
//! answers for each — `honoured`, the positioner's own placement, and `flipped`,
//! the same popup above its anchor — so the probe can name which one came back
//! rather than guess at a single expected number. Anything else is a `FAIL`.
//!
//! Which of the two is correct depends on the compositor. A tiling compositor
//! gives the window the whole usable area, so the second popup is anchored at
//! the bottom of the screen and has to flip. A floating one leaves room below
//! the window, and honouring the positioner is right there. Against Huginn,
//! expect `honoured` then `flipped`.

#[cfg(target_os = "linux")]
mod probe {
    use std::num::NonZeroU32;

    use smithay_client_toolkit::{
        compositor::{CompositorHandler, CompositorState},
        delegate_registry,
        output::{OutputHandler, OutputState},
        reexports::{
            client::{
                Connection, QueueHandle,
                globals::registry_queue_init,
                protocol::{wl_output, wl_shm, wl_surface},
            },
            protocols::xdg::shell::client::xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
        },
        registry::{ProvidesRegistryState, RegistryState},
        registry_handlers,
        shell::{
            WaylandSurface,
            xdg::{
                XdgPositioner, XdgShell, XdgSurface,
                popup::{ConfigureKind, Popup, PopupConfigure, PopupHandler},
                window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            },
        },
        shm::{Shm, ShmHandler, slot::SlotPool},
    };

    /// Popup size for both cases. The constrained case is deliberately taller
    /// than the gap below its anchor, which is what forces the flip.
    const POPUP: (i32, i32) = (200, 150);
    const TALL: (i32, i32) = (200, 400);
    /// The reposition token. Any non-zero value; the compositor must echo it.
    const TOKEN: u32 = 42;
    /// Height of the anchor rectangle, shared by both cases.
    const ANCHOR_H: i32 = 20;

    pub(crate) fn run() {
        let conn = Connection::connect_to_env()
            .expect("WAYLAND_DISPLAY is not set to a live socket");
        let (globals, mut queue) = registry_queue_init::<Probe>(&conn).expect("registry");
        let qh = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
        let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base");
        let shm = Shm::bind(&globals, &qh).expect("wl_shm");
        let pool = SlotPool::new(1920 * 1080 * 4, &shm).expect("shm pool");

        let surface = compositor.create_surface(&qh);
        let window =
            xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
        window.set_title("popup-probe");
        window.set_app_id("popup-probe");
        // No buffer before the first configure is acked: attaching one here is
        // the classic way to earn a protocol error instead of a window.
        window.commit();

        let mut probe = Probe {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            shm,
            pool,
            compositor,
            xdg_shell,
            window,
            popup: None,
            size: (0, 0),
            stage: Stage::Window,
            exit: false,
        };

        while !probe.exit {
            queue.blocking_dispatch(&mut probe).expect("dispatch");
        }
    }

    /// How far through the two cases the probe is. Each variant names the
    /// configure it is waiting for.
    #[derive(PartialEq, Eq)]
    enum Stage {
        /// The window's first configure, which nothing can start before.
        Window,
        /// The configure that answers the popup's creation.
        Fits,
        /// The configure that answers the reposition.
        Constrained,
    }

    struct Probe {
        registry_state: RegistryState,
        output_state: OutputState,
        shm: Shm,
        pool: SlotPool,
        compositor: CompositorState,
        xdg_shell: XdgShell,
        window: Window,
        popup: Option<Popup>,
        size: (i32, i32),
        stage: Stage,
        exit: bool,
    }

    impl Probe {
        /// Fill `surface` with a flat colour so the compositor has something to
        /// map. A popup with no buffer is never mapped, and an unmapped popup
        /// is indistinguishable from a popup the compositor placed wrongly.
        fn paint(&mut self, surface: &wl_surface::WlSurface, size: (i32, i32), colour: u32) {
            let (w, h) = size;
            let (buffer, canvas) = self
                .pool
                .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
                .expect("buffer");
            for pixel in canvas.chunks_exact_mut(4) {
                pixel.copy_from_slice(&colour.to_le_bytes());
            }
            surface.damage_buffer(0, 0, w, h);
            buffer.attach_to(surface).expect("attach");
            surface.commit();
        }

        /// A positioner anchored at `anchor`, opening down and to the right.
        ///
        /// `FlipY` is the only adjustment offered, so a popup that does not fit
        /// below its anchor has exactly one legal answer — above it — and the
        /// expected number in the table above is unambiguous.
        fn positioner(&self, anchor: (i32, i32, i32, i32), size: (i32, i32)) -> XdgPositioner {
            let positioner = XdgPositioner::new(&self.xdg_shell).expect("positioner");
            positioner.set_size(size.0, size.1);
            positioner.set_anchor_rect(anchor.0, anchor.1, anchor.2, anchor.3);
            positioner.set_anchor(Anchor::BottomLeft);
            positioner.set_gravity(Gravity::BottomRight);
            positioner.set_constraint_adjustment(ConstraintAdjustment::FlipY);
            positioner
        }
    }

    impl WindowHandler for Probe {
        fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
            self.exit = true;
        }

        fn configure(
            &mut self,
            _: &Connection,
            qh: &QueueHandle<Self>,
            _: &Window,
            configure: WindowConfigure,
            _serial: u32,
        ) {
            let (w, h) = configure.new_size;
            self.size = (
                w.map_or(600, NonZeroU32::get) as i32,
                h.map_or(400, NonZeroU32::get) as i32,
            );
            let surface = self.window.wl_surface().clone();
            self.paint(&surface, self.size, 0xFF20_2030);

            if self.stage != Stage::Window {
                return;
            }
            self.stage = Stage::Fits;

            // Case one: an anchor near the top, with room below it. Nothing
            // constrains this, so the compositor must return the positioner's
            // own answer untouched.
            let positioner = self.positioner((10, 10, 100, 20), POPUP);
            let popup = Popup::new(
                self.window.xdg_surface(),
                &positioner,
                qh,
                &self.compositor,
                &self.xdg_shell,
            )
            .expect("popup");
            self.popup = Some(popup);
        }
    }

    impl PopupHandler for Probe {
        fn configure(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            popup: &Popup,
            config: PopupConfigure,
        ) {
            let (x, y) = config.position;
            let h = self.size.1;
            let surface = popup.wl_surface().clone();

            match self.stage {
                Stage::Fits => {
                    report("fits", (x, y), 10, POPUP, self.size, &config);
                    self.paint(&surface, (config.width, config.height), 0xFF80_20A0);

                    // Case two: the same popup, re-anchored to the bottom of
                    // the window and made tall enough that it cannot open
                    // downwards. The compositor owes us a flip.
                    self.stage = Stage::Constrained;
                    let positioner = self.positioner((10, h - 30, 100, 20), TALL);
                    popup.reposition(&positioner, TOKEN);
                }
                Stage::Constrained => {
                    match config.kind {
                        ConfigureKind::Reposition { token } if token == TOKEN => {}
                        ref kind => println!("FAIL  reposition: expected token {TOKEN}, got {kind:?}"),
                    }
                    report("constrained", (x, y), h - 30, TALL, self.size, &config);
                    self.exit = true;
                }
                Stage::Window => {}
            }
        }

        fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Popup) {
            println!("FAIL  compositor dismissed the popup before it was placed");
            self.exit = true;
        }
    }

    /// Name which of the two legal placements came back, or fail.
    ///
    /// `anchor_y` is the top of the anchor rectangle in parent-window-geometry
    /// coordinates; the popup either opens from its bottom edge or is flipped
    /// to end at its top edge.
    fn report(
        case: &str,
        got: (i32, i32),
        anchor_y: i32,
        size: (i32, i32),
        window: (i32, i32),
        config: &PopupConfigure,
    ) {
        let honoured = (10, anchor_y + ANCHOR_H);
        let flipped = (10, anchor_y - size.1);
        let placement = match got {
            g if g == honoured => "honoured",
            g if g == flipped => "flipped",
            _ => "NEITHER",
        };
        let sized = (config.width, config.height) == size;
        let verdict = if placement == "NEITHER" || !sized { "FAIL" } else { "ok  " };
        println!(
            "{verdict}  {case:<12} {placement:<8} position {got:?}  \
             size {:?} (asked {size:?})  window {window:?}  \
             [honoured {honoured:?}, flipped {flipped:?}]",
            (config.width, config.height)
        );
    }

    impl CompositorHandler for Probe {
        fn scale_factor_changed(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_surface::WlSurface,
            _: i32,
        ) {
        }
        fn transform_changed(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_surface::WlSurface,
            _: wl_output::Transform,
        ) {
        }
        fn frame(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_surface::WlSurface,
            _: u32,
        ) {
        }
        fn surface_enter(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_surface::WlSurface,
            _: &wl_output::WlOutput,
        ) {
        }
        fn surface_leave(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_surface::WlSurface,
            _: &wl_output::WlOutput,
        ) {
        }
    }

    impl OutputHandler for Probe {
        fn output_state(&mut self) -> &mut OutputState {
            &mut self.output_state
        }
        fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
        fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
        fn output_destroyed(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: wl_output::WlOutput,
        ) {
        }
    }

    impl ShmHandler for Probe {
        fn shm_state(&mut self) -> &mut Shm {
            &mut self.shm
        }
    }

    impl ProvidesRegistryState for Probe {
        fn registry(&mut self) -> &mut RegistryState {
            &mut self.registry_state
        }
        registry_handlers![OutputState];
    }

    delegate_registry!(Probe);
    smithay_client_toolkit::delegate_dispatch2!(Probe);
}

#[cfg(target_os = "linux")]
fn main() {
    probe::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("popup-probe needs a Wayland compositor, so it only builds on Linux");
}
