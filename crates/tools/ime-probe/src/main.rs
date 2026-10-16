//! `ime-probe` — an input method for exercising text-input-v3 clients.
//!
//! IME support in a client is only reachable through a compositor relaying
//! `zwp_input_method_v2` requests to the focused `zwp_text_input_v3`, so it is
//! checked by being that input method: the probe binds the seat's input
//! method, waits until a focused client has enabled text input, then sends
//! the preedit and commit strings it was given, one step at a time.
//!
//! # Running it
//!
//! Start it before the client takes focus: Huginn activates an input method
//! when a text input is enabled, and a client enables on keyboard enter. With
//! `--timeout 0` the probe skips the wait and sends at once, which still
//! reaches a text input that is already enabled.
//!
//! ```sh
//! WAYLAND_DISPLAY=wayland-2 cargo run -p ime-probe -- \
//!     preedit nihon wait 800 preedit 日本 --cursor 1 wait 800 commit 日本
//! ```
//!
//! `key` steps go through `zwp_virtual_keyboard_v1`, with the seat's own
//! keymap, when the compositor offers it.
//!
//! Exit codes: 0 done, 1 a required global is missing, 2 bad command line,
//! 3 another input method holds the seat, 4 no text input became active.

mod plan;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ime-probe needs a Wayland session; build it on the RavenLinux host");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::ErrorKind;
    use std::os::fd::{AsFd, OwnedFd};
    use std::process::exit;
    use std::time::{Duration, Instant};

    use wayland_client::{
        Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
        backend::WaylandError,
        globals::{GlobalList, GlobalListContents, registry_queue_init},
        protocol::{wl_keyboard, wl_registry, wl_seat},
    };
    use wayland_protocols_misc::zwp_input_method_v2::client::{
        zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        zwp_input_method_v2::{self, ZwpInputMethodV2},
    };
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };

    use crate::plan::{self, Op, Plan};

    #[derive(Default)]
    struct Probe {
        /// `done` events so far: the serial every `commit` must carry.
        done: u32,
        pending_active: bool,
        active: bool,
        unavailable: bool,
        keymap: Option<(OwnedFd, u32)>,
    }

    pub(crate) fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let plan = plan::parse_args(&args).unwrap_or_else(|e| {
            eprintln!("{e}");
            exit(2)
        });

        let conn = Connection::connect_to_env().unwrap_or_else(|e| {
            eprintln!("probe: no Wayland display ({e}); set WAYLAND_DISPLAY");
            exit(1)
        });
        let (globals, mut queue) = registry_queue_init::<Probe>(&conn).expect("registry");
        let qh = queue.handle();
        let mut probe = Probe::default();

        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap_or_else(|_| {
            println!("probe: no wl_seat");
            exit(1)
        });
        let manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ()).unwrap_or_else(|_| {
            println!(
                "probe: no zwp_input_method_manager_v2 — this compositor has no input methods"
            );
            exit(1)
        });
        println!(
            "probe: bound zwp_input_method_manager_v2 v{}",
            manager.version()
        );
        let im = manager.get_input_method(&seat, &qh, ());

        let vk = plan
            .needs_virtual_keyboard()
            .then(|| virtual_keyboard(&conn, &mut queue, &mut probe, &globals, &seat));

        conn.roundtrip().expect("roundtrip");
        queue.dispatch_pending(&mut probe).expect("dispatch");
        if probe.unavailable {
            println!("probe: unavailable — another input method holds the seat");
            exit(3);
        }
        println!("probe: bound input method");

        execute(&plan, &conn, &mut queue, &mut probe, &im, vk.as_ref());

        conn.roundtrip().expect("roundtrip");
        im.destroy();
        let _ = conn.flush();
        println!("probe: done");
    }

    fn execute(
        plan: &Plan,
        conn: &Connection,
        queue: &mut EventQueue<Probe>,
        probe: &mut Probe,
        im: &ZwpInputMethodV2,
        vk: Option<&ZwpVirtualKeyboardV1>,
    ) {
        let start = Instant::now();
        for op in &plan.ops {
            if op.needs_input_method() && !plan.timeout.is_zero() && !probe.active {
                println!("probe: waiting for a focused text input to enable");
                let until = Instant::now() + plan.timeout;
                pump(conn, queue, probe, until, |p| p.active || p.unavailable);
                if probe.unavailable {
                    println!("probe: unavailable — another input method holds the seat");
                    exit(3);
                }
                if !probe.active {
                    println!(
                        "probe: no text input became active within {} ms",
                        plan.timeout.as_millis()
                    );
                    exit(4);
                }
            }
            queue.dispatch_pending(probe).expect("dispatch");
            match op {
                Op::Preedit { text, begin, end } => {
                    im.set_preedit_string(text.clone(), *begin, *end);
                    im.commit(probe.done);
                    println!("probe: sent preedit {text:?} cursor {begin}..{end}");
                }
                Op::Commit(text) => {
                    im.commit_string(text.clone());
                    im.commit(probe.done);
                    println!("probe: sent commit {text:?}");
                }
                Op::Delete { before, after } => {
                    im.delete_surrounding_text(*before, *after);
                    im.commit(probe.done);
                    println!("probe: sent delete {before} before, {after} after");
                }
                Op::Wait(d) => pump(conn, queue, probe, Instant::now() + *d, |_| false),
                Op::Key { code, mods } => {
                    let vk = vk.expect("planned keys bind a virtual keyboard");
                    let time = start.elapsed().as_millis() as u32;
                    if *mods != 0 {
                        vk.modifiers(*mods, 0, 0, 0);
                    }
                    vk.key(time, *code, 1);
                    vk.key(time + 1, *code, 0);
                    if *mods != 0 {
                        vk.modifiers(0, 0, 0, 0);
                    }
                    println!("probe: sent key {code} mods {mods:#x}");
                }
            }
            conn.flush().expect("flush");
        }
    }

    /// Bind the virtual keyboard and hand it the seat's own keymap, so key
    /// codes mean what they mean on the real keyboard.
    fn virtual_keyboard(
        conn: &Connection,
        queue: &mut EventQueue<Probe>,
        probe: &mut Probe,
        globals: &GlobalList,
        seat: &wl_seat::WlSeat,
    ) -> ZwpVirtualKeyboardV1 {
        let qh = queue.handle();
        let manager: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).unwrap_or_else(|_| {
                println!("probe: no zwp_virtual_keyboard_manager_v1 — key steps are unavailable");
                exit(1)
            });
        let keyboard = seat.get_keyboard(&qh, ());
        let until = Instant::now() + Duration::from_secs(5);
        pump(conn, queue, probe, until, |p| p.keymap.is_some());
        let Some((fd, size)) = probe.keymap.take() else {
            println!("probe: the seat sent no keymap");
            exit(1)
        };
        keyboard.release();
        let vk = manager.create_virtual_keyboard(seat, &qh, ());
        vk.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), fd.as_fd(), size);
        println!("probe: bound zwp_virtual_keyboard_manager_v1");
        vk
    }

    /// Dispatch events until `cond` holds or `until` passes.
    fn pump(
        conn: &Connection,
        queue: &mut EventQueue<Probe>,
        probe: &mut Probe,
        until: Instant,
        cond: impl Fn(&Probe) -> bool,
    ) {
        loop {
            queue.dispatch_pending(probe).expect("dispatch");
            if cond(probe) || Instant::now() >= until {
                return;
            }
            conn.flush().expect("flush");
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    println!("probe: connection lost: {e}");
                    exit(1);
                }
            }
        }
    }

    impl Dispatch<ZwpInputMethodV2, ()> for Probe {
        fn event(
            state: &mut Self,
            _: &ZwpInputMethodV2,
            event: zwp_input_method_v2::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            use zwp_input_method_v2::Event;
            match event {
                Event::Activate => state.pending_active = true,
                Event::Deactivate => state.pending_active = false,
                Event::SurroundingText {
                    text,
                    cursor,
                    anchor,
                } => {
                    println!("probe: surrounding {text:?} cursor {cursor} anchor {anchor}");
                }
                Event::Done => {
                    state.done += 1;
                    if state.active != state.pending_active {
                        state.active = state.pending_active;
                        println!(
                            "probe: {}",
                            if state.active { "active" } else { "inactive" }
                        );
                    }
                }
                Event::Unavailable => state.unavailable = true,
                _ => {}
            }
        }
    }

    impl Dispatch<wl_keyboard::WlKeyboard, ()> for Probe {
        fn event(
            state: &mut Self,
            _: &wl_keyboard::WlKeyboard,
            event: wl_keyboard::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } = event
            {
                state.keymap = Some((fd, size));
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    macro_rules! ignore_events {
        ($($t:ty),*) => {$(
            impl Dispatch<$t, ()> for Probe {
                fn event(
                    _: &mut Self,
                    _: &$t,
                    _: <$t as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*};
    }

    ignore_events!(
        wl_seat::WlSeat,
        ZwpInputMethodManagerV2,
        ZwpVirtualKeyboardManagerV1,
        ZwpVirtualKeyboardV1
    );
}
