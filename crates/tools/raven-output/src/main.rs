//! raven-output: where the screens are, and where to put them.
//!
//! The client side of `raven_output_layout_v1`. Huginn keeps the arrangement;
//! this is how a person changes it from a terminal, and the reference for a
//! settings page that does the same with a picture.
//!
//! ```sh
//! raven-output                          # list every screen
//! raven-output move HDMI-A-1 0 -1440    # put its top-left corner there
//! raven-output above HDMI-A-1 eDP-1     # the monitor above the laptop panel
//! raven-output right-of DP-1 eDP-1
//! raven-output scale eDP-1 1.5          # lay it out at 1.5x
//! raven-output scale eDP-1 auto         # back to what its size implies
//! ```
//!
//! Every change is applied at once and saved by the compositor, and the
//! resulting layout is printed: what was asked for is pushed aside if it
//! would overlap, so the answer is the truth rather than the request.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("raven-output talks to a Wayland compositor; build it on the RavenLinux host");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use raven_protocol::client::{
        raven_output_layout_v1::{self, RavenOutputLayoutV1},
        raven_shell_manager_v1::RavenShellManagerV1,
    };
    use wayland_client::{
        Connection, Dispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };

    #[derive(Debug, Clone)]
    struct Screen {
        name: String,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        scale: f64,
        physical: (i32, i32),
        mm: (i32, i32),
        focused: bool,
    }

    #[derive(Debug, Default)]
    struct App {
        screens: Vec<Screen>,
        /// One full set has arrived since the last time this was cleared.
        done: bool,
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
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

    impl Dispatch<RavenShellManagerV1, ()> for App {
        fn event(
            _: &mut Self,
            _: &RavenShellManagerV1,
            _: <RavenShellManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<RavenOutputLayoutV1, ()> for App {
        fn event(
            app: &mut Self,
            _: &RavenOutputLayoutV1,
            event: raven_output_layout_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                raven_output_layout_v1::Event::Output {
                    name,
                    x,
                    y,
                    width,
                    height,
                    scale,
                    physical_width,
                    physical_height,
                    mm_width,
                    mm_height,
                    focused,
                } => {
                    if app.done {
                        // A new set is starting.
                        app.screens.clear();
                        app.done = false;
                    }
                    app.screens.push(Screen {
                        name,
                        x,
                        y,
                        width,
                        height,
                        scale,
                        physical: (physical_width, physical_height),
                        mm: (mm_width, mm_height),
                        focused: focused == 1,
                    });
                }
                raven_output_layout_v1::Event::Done => app.done = true,
                _ => {}
            }
        }
    }

    fn usage() -> ! {
        eprintln!(
            "usage: raven-output                         list the screens\n       \
             raven-output move NAME X Y            put NAME's top-left corner at X,Y\n       \
             raven-output left-of|right-of|above|below NAME OTHER\n       \
             raven-output scale NAME FACTOR|auto   lay NAME out at FACTOR (e.g. 1.5)"
        );
        std::process::exit(2);
    }

    pub(super) fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();

        let conn = Connection::connect_to_env()
            .expect("no Wayland display; set WAYLAND_DISPLAY to huginn's socket");
        let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry");
        let qh = queue.handle();
        let manager: RavenShellManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .expect("raven_shell_manager_v1 version 3: is this huginn, and is it recent?");
        let layout = manager.get_output_layout(&qh, ());

        let mut app = App::default();
        wait_for_done(&mut queue, &mut app);

        let (command, rest) = match args.split_first() {
            None => ("list", &args[..]),
            Some((first, rest)) => (first.as_str(), rest),
        };
        match command {
            "list" => {
                print(&app.screens);
                return;
            }
            "move" => {
                let [name, x, y] = rest else { usage() };
                let (Ok(x), Ok(y)) = (x.parse::<i32>(), y.parse::<i32>()) else {
                    usage()
                };
                layout.set_position(name.clone(), x, y);
            }
            "left-of" | "right-of" | "above" | "below" => {
                let [name, other] = rest else { usage() };
                let Some(me) = app.screens.iter().find(|s| &s.name == name) else {
                    eprintln!("raven-output: no screen named {name}");
                    std::process::exit(1);
                };
                let Some(anchor) = app.screens.iter().find(|s| &s.name == other) else {
                    eprintln!("raven-output: no screen named {other}");
                    std::process::exit(1);
                };
                let (x, y) = match command {
                    "left-of" => (anchor.x - me.width, anchor.y),
                    "right-of" => (anchor.x + anchor.width, anchor.y),
                    "above" => (anchor.x, anchor.y - me.height),
                    _ => (anchor.x, anchor.y + anchor.height),
                };
                // The anchor keeps its place explicitly, so it is the one
                // that wins if the two are ever in conflict.
                layout.set_position(other.clone(), anchor.x, anchor.y);
                layout.set_position(name.clone(), x, y);
            }
            "scale" => {
                let [name, factor] = rest else { usage() };
                let scale = if factor == "auto" {
                    0.0
                } else {
                    match factor.parse::<f64>() {
                        Ok(f) if f > 0.0 => f,
                        _ => usage(),
                    }
                };
                layout.set_scale(name.clone(), scale);
            }
            _ => usage(),
        }
        layout.apply();
        // The compositor answers with the layout it actually arrived at.
        app.done = false;
        wait_for_done(&mut queue, &mut app);
        print(&app.screens);
    }

    fn wait_for_done(queue: &mut wayland_client::EventQueue<App>, app: &mut App) {
        while !app.done {
            queue.blocking_dispatch(app).expect("dispatch");
        }
    }

    fn print(screens: &[Screen]) {
        if screens.is_empty() {
            println!("no screens");
            return;
        }
        for s in screens {
            let inches = if s.mm.0 > 0 && s.mm.1 > 0 {
                let diagonal = f64::from(s.mm.0).hypot(f64::from(s.mm.1)) / 25.4;
                format!("{diagonal:.1}\"")
            } else {
                "size unknown".to_owned()
            };
            println!(
                "{:<10} {:>5},{:<5} {:>5}x{:<5} logical  {}x{} panel  {}  scale {:.2}{}",
                s.name,
                s.x,
                s.y,
                s.width,
                s.height,
                s.physical.0,
                s.physical.1,
                inches,
                s.scale,
                if s.focused { "  focused" } else { "" }
            );
        }
    }
}
