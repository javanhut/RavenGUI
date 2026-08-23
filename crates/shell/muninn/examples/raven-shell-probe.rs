//! Probe for `raven_shell_v1`.
//!
//! Connects to a compositor, prints every workspace-state event, and optionally
//! sends an `activate` request. Useful both as an integration test of the
//! protocol and as a debugging tool when the panel is not doing what you
//! expect and you need to know whether the fault is Huginn's or Muninn's.
//!
//! ```sh
//! WAYLAND_DISPLAY=huginn-1 cargo run -p muninn --example raven-shell-probe
//! WAYLAND_DISPLAY=huginn-1 cargo run -p muninn --example raven-shell-probe -- 3
//! ```

#[cfg(target_os = "linux")]
mod probe {
    use raven_protocol::client::{
        raven_shell_manager_v1::{self, RavenShellManagerV1},
        raven_workspace_state_v1::{self, RavenWorkspaceStateV1},
    };
    use smithay_client_toolkit::reexports::client::{
        Connection, Dispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };

    struct Probe {
        activate: Option<u32>,
        seen: u32,
    }

    pub(crate) fn run() {
        // Optional argument: a workspace index to activate after the first event.
        let activate = std::env::args().nth(1).and_then(|a| a.parse::<u32>().ok());

        let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY is not set to a live socket");
        let (globals, mut queue) = registry_queue_init::<Probe>(&conn).expect("registry");
        let qh = queue.handle();

        let manager = globals
            .bind::<RavenShellManagerV1, _, _>(&qh, 1..=1, ())
            .expect("compositor does not implement raven_shell_v1");
        let _state = manager.get_workspace_state(&qh, ());

        let mut probe = Probe { activate, seen: 0 };

        // Two round trips: one for the initial state, one for the state that
        // follows an activate. Without an activate, one is enough.
        let rounds = if activate.is_some() { 2 } else { 1 };
        while probe.seen < rounds {
            queue.blocking_dispatch(&mut probe).expect("dispatch");
        }
    }

    impl Dispatch<RavenWorkspaceStateV1, ()> for Probe {
        fn event(
            state: &mut Self,
            proxy: &RavenWorkspaceStateV1,
            event: raven_workspace_state_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let raven_workspace_state_v1::Event::State {
                count,
                active,
                occupied,
            } = event
            else {
                return;
            };
            println!("state: count={count} active={active} occupied={occupied:0count$b}", count = count as usize);
            state.seen += 1;

            if let Some(index) = state.activate.take() {
                println!("-> activate({index})");
                proxy.activate(index);
            }
        }
    }

    impl Dispatch<RavenShellManagerV1, ()> for Probe {
        fn event(
            _: &mut Self,
            _: &RavenShellManagerV1,
            _: raven_shell_manager_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
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
}

#[cfg(target_os = "linux")]
fn main() {
    probe::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("raven-shell-probe needs a Wayland session; run it on the RavenLinux host");
    std::process::exit(1);
}
