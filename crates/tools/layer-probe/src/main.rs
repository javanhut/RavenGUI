//! `layer-probe` — a layer-shell client for exercising Huginn by hand.
//!
//! The compositor's panel behaviour is decided in `huginn-comp`, in glue that
//! needs a live `Huginn`, a real seat and a client attaching and detaching
//! buffers. None of that is reachable from a unit test, so the parts of it that
//! matter are checked by driving a real client against a real compositor. This
//! is that client.
//!
//! It is deliberately not a panel. It draws a flat band, says on stdout what the
//! compositor did to it, and changes colour when it holds the keyboard, so that
//! the four behaviours below can be told apart by looking.
//!
//! # What it is for
//!
//! - **Keyboard interactivity.** `--interactivity exclusive` should take the
//!   keyboard the moment the surface maps; `on-demand` should take it on a click
//!   and give it back on a click elsewhere; `none` should never take it. Every
//!   `enter`, `leave` and key is printed.
//! - **The focus ring.** While this holds the keyboard *exclusively*, the ring
//!   around the focused window is supposed to retire. On-demand focus leaves it
//!   alone.
//! - **Unmapping.** `--cycle N` detaches the buffer every N seconds and attaches
//!   it again. An unmapped surface must lose the keyboard and stop being
//!   clickable, and must get it back when it maps again.
//! - **The exclusive zone.** Space is reserved only while something is on
//!   screen, so the tiled windows behind this should grow into the strip while
//!   it is unmapped and give it back when it returns.
//!
//! # Running it
//!
//! Against a nested compositor, which is where this belongs:
//!
//! ```sh
//! cargo run -p huginn-comp                       # prints its socket
//! WAYLAND_DISPLAY=wayland-2 cargo run -p layer-probe -- --interactivity exclusive
//! WAYLAND_DISPLAY=wayland-2 cargo run -p layer-probe -- --cycle 3
//! ```
//!
//! `Esc` exits, and so does the compositor closing the surface.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("layer-probe needs a Wayland session; build it on the RavenLinux host");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use smithay_client_toolkit::{
        compositor::{CompositorHandler, CompositorState},
        delegate_registry,
        output::{OutputHandler, OutputState},
        registry::{ProvidesRegistryState, RegistryState},
        registry_handlers,
        seat::{
            Capability, SeatHandler, SeatState,
            keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
            pointer::{PointerEvent, PointerEventKind, PointerHandler},
        },
        shell::{
            WaylandSurface,
            wlr_layer::{
                Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
                LayerSurfaceConfigure,
            },
        },
        shm::{Shm, ShmHandler, slot::SlotPool},
    };
    use wayland_client::{
        Connection, QueueHandle,
        globals::registry_queue_init,
        protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    };

    /// Huginn's own colours, so the probe sits on the desktop rather than on top
    /// of it. Unfocused is the panel background; focused is the accent, which is
    /// the point of the whole exercise being visible from across the room.
    const UNFOCUSED: u32 = 0xFF16_161F;
    const FOCUSED: u32 = 0xFF7A_A2F7;
    const BORDER: u32 = 0xFF2A_2A3A;

    /// What was asked for on the command line.
    struct Args {
        layer: Layer,
        interactivity: KeyboardInteractivity,
        anchor: Anchor,
        vertical: bool,
        thickness: u32,
        exclusive: i32,
        cycle: Option<Duration>,
    }

    impl Default for Args {
        fn default() -> Self {
            Self {
                layer: Layer::Top,
                interactivity: KeyboardInteractivity::OnDemand,
                // Spanning the edge, not just touching it: a zero in either
                // dimension of `set_size` means "you decide", and the protocol
                // only allows that on an axis the surface is anchored across.
                anchor: Anchor::BOTTOM.union(Anchor::LEFT).union(Anchor::RIGHT),
                vertical: false,
                thickness: 64,
                exclusive: -1,
                cycle: None,
            }
        }
    }

    fn usage() -> ! {
        eprintln!(
            "layer-probe — drive Huginn's panel behaviour by hand

  --layer         background | bottom | top | overlay   (default top)
  --interactivity none | on-demand | exclusive          (default on-demand)
  --anchor        top | bottom | left | right           (default bottom)
  --size N        thickness in pixels                   (default 64)
  --exclusive N   pixels to reserve, 0 for none         (default: same as size)
  --cycle N       unmap and remap every N seconds       (default: never)

Esc exits."
        );
        std::process::exit(2)
    }

    fn parse() -> Args {
        let mut args = Args::default();
        let mut exclusive_set = false;
        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            let mut value = || argv.next().unwrap_or_else(|| usage());
            match flag.as_str() {
                "--layer" => {
                    args.layer = match value().as_str() {
                        "background" => Layer::Background,
                        "bottom" => Layer::Bottom,
                        "top" => Layer::Top,
                        "overlay" => Layer::Overlay,
                        _ => usage(),
                    }
                }
                "--interactivity" => {
                    args.interactivity = match value().as_str() {
                        "none" => KeyboardInteractivity::None,
                        "on-demand" => KeyboardInteractivity::OnDemand,
                        "exclusive" => KeyboardInteractivity::Exclusive,
                        _ => usage(),
                    }
                }
                "--anchor" => {
                    let (anchor, vertical) = match value().as_str() {
                        "top" => (Anchor::TOP | Anchor::LEFT | Anchor::RIGHT, false),
                        "bottom" => (Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT, false),
                        "left" => (Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM, true),
                        "right" => (Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM, true),
                        _ => usage(),
                    };
                    args.anchor = anchor;
                    args.vertical = vertical;
                }
                "--size" => args.thickness = value().parse().unwrap_or_else(|_| usage()),
                "--exclusive" => {
                    args.exclusive = value().parse().unwrap_or_else(|_| usage());
                    exclusive_set = true;
                }
                "--cycle" => {
                    let secs: u64 = value().parse().unwrap_or_else(|_| usage());
                    args.cycle = (secs > 0).then(|| Duration::from_secs(secs));
                }
                "--help" | "-h" => usage(),
                _ => usage(),
            }
        }
        // Reserving exactly what it draws is what a panel does, so that is the
        // default; `--exclusive` is for asking a panel-shaped surface to reserve
        // nothing, which is the interesting case for hit-testing.
        if !exclusive_set {
            args.exclusive = args.thickness as i32;
        }
        args
    }

    pub(crate) fn run() {
        let args = parse();

        let conn = Connection::connect_to_env()
            .expect("no Wayland display; set WAYLAND_DISPLAY to the compositor's socket");
        let (globals, mut queue) = registry_queue_init(&conn).expect("registry");
        let qh = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
        let shell = LayerShell::bind(&globals, &qh)
            .expect("zwlr_layer_shell_v1 — this compositor has no layer shell");
        let shm = Shm::bind(&globals, &qh).expect("wl_shm");

        let surface = compositor.create_surface(&qh);
        let layer = shell.create_layer_surface(&qh, surface, args.layer, Some("layer-probe"), None);
        layer.set_anchor(args.anchor);
        layer.set_keyboard_interactivity(args.interactivity);
        layer.set_exclusive_zone(args.exclusive);
        if args.vertical {
            layer.set_size(args.thickness, 0);
        } else {
            layer.set_size(0, args.thickness);
        }

        // The zone is declared here, before any buffer exists. Huginn reserves
        // nothing for it until the first buffer arrives, which is the behaviour
        // this line is here to exercise: the desktop should not make room yet.
        println!(
            "probe: {:?} layer, {:?}, anchor {:?}, {}px, exclusive {}",
            args.layer, args.interactivity, args.anchor, args.thickness, args.exclusive
        );
        println!("probe: committed with no buffer — the desktop should not have made room yet");
        layer.commit();

        let pool = SlotPool::new(1920 * 64 * 4, &shm).expect("shm pool");

        let mut probe = Probe {
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state: OutputState::new(&globals, &qh),
            shm,
            pool,
            layer,
            width: 0,
            height: 0,
            configured: false,
            mapped: false,
            focused: false,
            exit: false,
            keyboard: None,
            pointer: None,
        };

        // A thread to drive `--cycle`, and a roundtrip to wake the main loop out
        // of `blocking_dispatch` when it fires. An unmapped surface receives no
        // events of its own, so without something arriving on the socket the
        // loop would sleep through the remap it is waiting to perform.
        let due = Arc::new(AtomicBool::new(false));
        if let Some(period) = args.cycle {
            let due = Arc::clone(&due);
            let conn = conn.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(period);
                    due.store(true, Ordering::SeqCst);
                    let _ = conn.roundtrip();
                }
            });
        }

        while !probe.exit {
            queue.blocking_dispatch(&mut probe).expect("dispatch");
            if due.swap(false, Ordering::SeqCst) {
                probe.toggle_map(&qh);
            }
        }
        println!("probe: exiting");
    }

    struct Probe {
        registry_state: RegistryState,
        seat_state: SeatState,
        output_state: OutputState,
        shm: Shm,
        pool: SlotPool,
        layer: LayerSurface,
        width: u32,
        height: u32,
        configured: bool,
        /// Whether a buffer is attached. Tracked here because the whole point of
        /// `--cycle` is to be able to say which side of the transition we are on
        /// when the compositor reacts.
        mapped: bool,
        focused: bool,
        exit: bool,
        keyboard: Option<wl_keyboard::WlKeyboard>,
        pointer: Option<wl_pointer::WlPointer>,
    }

    impl Probe {
        /// Attach a buffer, or take it away, and say which.
        fn toggle_map(&mut self, qh: &QueueHandle<Self>) {
            if self.mapped {
                println!("probe: unmapping — expect focus released and the zone given back");
                self.layer.wl_surface().attach(None, 0, 0);
                self.layer.commit();
                self.mapped = false;
            } else {
                println!("probe: remapping — expect the zone reserved again");
                self.draw(qh);
            }
        }

        /// Paint the band and attach it. Nothing here asks for a frame callback:
        /// an idle probe should leave the compositor idle too, so that a busy
        /// compositor during a run means something.
        fn draw(&mut self, _qh: &QueueHandle<Self>) {
            let (w, h) = (self.width.max(1), self.height.max(1));
            let stride = w as i32 * 4;
            let Ok((buffer, canvas)) =
                self.pool
                    .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            else {
                eprintln!("probe: could not allocate a {w}x{h} buffer");
                return;
            };

            let fill = if self.focused { FOCUSED } else { UNFOCUSED };
            for (index, chunk) in canvas.chunks_exact_mut(4).enumerate() {
                let x = index as u32 % w;
                let y = index as u32 / w;
                let edge = x < 2 || y < 2 || x + 2 >= w || y + 2 >= h;
                let color = if edge { BORDER } else { fill };
                chunk.copy_from_slice(&color.to_le_bytes());
            }

            self.layer
                .wl_surface()
                .damage_buffer(0, 0, w as i32, h as i32);
            if buffer.attach_to(self.layer.wl_surface()).is_err() {
                eprintln!("probe: buffer attach failed");
                return;
            }
            self.layer.commit();
            self.mapped = true;
        }

        /// Redraw only when there is something to redraw with.
        fn refresh(&mut self, qh: &QueueHandle<Self>) {
            if self.configured && self.mapped {
                self.draw(qh);
            }
        }
    }

    impl LayerShellHandler for Probe {
        fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
            println!("probe: the compositor closed the surface");
            self.exit = true;
        }

        fn configure(
            &mut self,
            _: &Connection,
            qh: &QueueHandle<Self>,
            _: &LayerSurface,
            configure: LayerSurfaceConfigure,
            _serial: u32,
        ) {
            let (w, h) = configure.new_size;
            println!("probe: configure {w}x{h}");
            self.width = w;
            self.height = h;
            if !self.configured {
                self.configured = true;
                println!("probe: attaching the first buffer — the desktop should make room now");
            }
            self.draw(qh);
        }
    }

    impl KeyboardHandler for Probe {
        fn enter(
            &mut self,
            _: &Connection,
            qh: &QueueHandle<Self>,
            _: &wl_keyboard::WlKeyboard,
            surface: &wl_surface::WlSurface,
            _: u32,
            _: &[u32],
            _: &[Keysym],
        ) {
            if self.layer.wl_surface() == surface {
                println!("probe: KEYBOARD ENTER — this surface now holds the keyboard");
                self.focused = true;
                self.refresh(qh);
            }
        }

        fn leave(
            &mut self,
            _: &Connection,
            qh: &QueueHandle<Self>,
            _: &wl_keyboard::WlKeyboard,
            surface: &wl_surface::WlSurface,
            _: u32,
        ) {
            if self.layer.wl_surface() == surface {
                println!("probe: KEYBOARD LEAVE — the keyboard went elsewhere");
                self.focused = false;
                self.refresh(qh);
            }
        }

        fn press_key(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_keyboard::WlKeyboard,
            _: u32,
            event: KeyEvent,
        ) {
            println!(
                "probe: key {:?} ({})",
                event.keysym,
                event.utf8.as_deref().unwrap_or("")
            );
            if event.keysym == Keysym::Escape {
                self.exit = true;
            }
        }

        fn repeat_key(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_keyboard::WlKeyboard,
            _: u32,
            event: KeyEvent,
        ) {
            println!("probe: key repeat {:?}", event.keysym);
        }

        fn release_key(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_keyboard::WlKeyboard,
            _: u32,
            _: KeyEvent,
        ) {
        }

        fn update_modifiers(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_keyboard::WlKeyboard,
            _: u32,
            _: Modifiers,
            _: RawModifiers,
            _: u32,
        ) {
        }
    }

    impl PointerHandler for Probe {
        fn pointer_frame(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_pointer::WlPointer,
            events: &[PointerEvent],
        ) {
            for event in events {
                if &event.surface != self.layer.wl_surface() {
                    continue;
                }
                if let PointerEventKind::Press { button, .. } = event.kind {
                    println!(
                        "probe: click {button:#x} at {:?} — on-demand should take focus here",
                        event.position
                    );
                }
            }
        }
    }

    impl CompositorHandler for Probe {
        fn scale_factor_changed(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: &wl_surface::WlSurface,
            factor: i32,
        ) {
            println!("probe: scale {factor}");
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

    impl SeatHandler for Probe {
        fn seat_state(&mut self) -> &mut SeatState {
            &mut self.seat_state
        }

        fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

        fn new_capability(
            &mut self,
            _: &Connection,
            qh: &QueueHandle<Self>,
            seat: wl_seat::WlSeat,
            capability: Capability,
        ) {
            if capability == Capability::Keyboard && self.keyboard.is_none() {
                self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            }
            if capability == Capability::Pointer && self.pointer.is_none() {
                self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            }
        }

        fn remove_capability(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: wl_seat::WlSeat,
            capability: Capability,
        ) {
            if capability == Capability::Keyboard
                && let Some(k) = self.keyboard.take()
            {
                k.release();
            }
            if capability == Capability::Pointer
                && let Some(p) = self.pointer.take()
            {
                p.release();
            }
        }

        fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    }

    impl OutputHandler for Probe {
        fn output_state(&mut self) -> &mut OutputState {
            &mut self.output_state
        }
        fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
        fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        }
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

    delegate_registry!(Probe);

    impl ProvidesRegistryState for Probe {
        fn registry(&mut self) -> &mut RegistryState {
            &mut self.registry_state
        }
        registry_handlers![OutputState, SeatState];
    }

    smithay_client_toolkit::delegate_dispatch2!(Probe);
}
