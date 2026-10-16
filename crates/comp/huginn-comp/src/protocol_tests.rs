//! Protocol tests against a real `Huginn` over a socket pair, with a
//! `wayland-client` connection on the other end.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use huginn_core::geometry::Rect;
use smithay::reexports::wayland_server::{
    Client, Display, protocol::wl_surface::WlSurface as SWlSurface,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child,
    protocol::{wl_compositor, wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::{
    wp::primary_selection::zv1::client::{
        zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
        zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
        zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
    },
    wp::text_input::zv3::client::{
        zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        zwp_text_input_v3::{self, ZwpTextInputV3},
    },
    xdg::activation::v1::client::{
        xdg_activation_token_v1::{self, XdgActivationTokenV1},
        xdg_activation_v1::XdgActivationV1,
    },
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};

use crate::activation::Origin;
use crate::focus::FocusTarget;
use crate::state::{ClientState, Huginn};

#[derive(Default)]
struct Probe {
    globals: Vec<(u32, String, u32)>,
    offered: Vec<String>,
    selections: usize,
    token: Option<String>,
    text_input_entered: bool,
    ime_activated: bool,
    ime_done: bool,
}

struct Peer {
    conn: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
    registry: wl_registry::WlRegistry,
}

struct Harness {
    display: Display<Huginn>,
    state: Huginn,
}

impl Harness {
    fn new() -> Self {
        let display = Display::<Huginn>::new().expect("display");
        let state = Huginn::new(&display.handle(), Rect::from_xywh(0, 0, 800, 600));
        Self { display, state }
    }

    fn connect(&mut self) -> Peer {
        let (server, client) = UnixStream::pair().expect("socket pair");
        self.display
            .handle()
            .insert_client(server, Arc::new(ClientState::default()))
            .expect("insert client");
        let conn = Connection::from_socket(client).expect("client connection");
        let queue = conn.new_event_queue();
        let registry = conn.display().get_registry(&queue.handle(), ());
        let mut peer = Peer {
            conn,
            queue,
            probe: Probe::default(),
            registry,
        };
        self.roundtrip(&mut peer);
        peer
    }

    fn roundtrip(&mut self, peer: &mut Peer) {
        for _ in 0..4 {
            peer.conn.flush().expect("client flush");
            self.display
                .dispatch_clients(&mut self.state)
                .expect("dispatch");
            self.display.flush_clients().expect("server flush");
            if let Some(guard) = peer.conn.prepare_read() {
                let _ = guard.read();
            }
            peer.queue
                .dispatch_pending(&mut peer.probe)
                .expect("client dispatch");
        }
    }
}

impl Peer {
    fn bind<I: Proxy + 'static>(&self, version: u32) -> I
    where
        Probe: Dispatch<I, ()>,
    {
        let (name, _, max) = self
            .probe
            .globals
            .iter()
            .find(|(_, interface, _)| interface == I::interface().name)
            .unwrap_or_else(|| panic!("{} is not advertised", I::interface().name))
            .clone();
        self.registry
            .bind::<I, (), Probe>(name, version.min(max), &self.queue.handle(), ())
    }

    fn has(&self, interface: &str) -> bool {
        self.probe
            .globals
            .iter()
            .any(|(_, name, _)| name == interface)
    }
}

fn origin_of(state: &Huginn, token: &str) -> Option<Origin> {
    state
        .activation_state
        .tokens()
        .find(|(t, _)| t.as_str() == token)
        .map(|(_, data)| {
            data.user_data
                .get::<Origin>()
                .copied()
                .unwrap_or(Origin::Untrusted)
        })
}

const ADVERTISED: &[&str] = &[
    "zwp_primary_selection_device_manager_v1",
    "zwp_text_input_manager_v3",
    "zwp_input_method_manager_v2",
    "xdg_activation_v1",
];

#[test]
fn the_new_globals_are_advertised() {
    let mut h = Harness::new();
    let peer = h.connect();
    for name in ADVERTISED {
        assert!(peer.has(name), "{name} is not advertised");
    }
}

#[test]
fn primary_selection_follows_keyboard_focus() {
    let mut h = Harness::new();
    h.state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .expect("keyboard");

    let (server_a, client_a) = UnixStream::pair().expect("pair");
    let (server_b, client_b) = UnixStream::pair().expect("pair");
    let id_a = h
        .display
        .handle()
        .insert_client(server_a, Arc::new(ClientState::default()))
        .expect("a");
    let id_b = h
        .display
        .handle()
        .insert_client(server_b, Arc::new(ClientState::default()))
        .expect("b");
    let mut a = peer_on(&mut h, client_a);
    let mut b = peer_on(&mut h, client_b);

    let setup = |h: &mut Harness, peer: &mut Peer| {
        let compositor = peer.bind::<wl_compositor::WlCompositor>(4);
        let seat = peer.bind::<wl_seat::WlSeat>(7);
        let manager = peer.bind::<ZwpPrimarySelectionDeviceManagerV1>(1);
        let qh = peer.queue.handle();
        let surface = compositor.create_surface(&qh, ());
        let device = manager.get_device(&seat, &qh, ());
        h.roundtrip(peer);
        (surface, manager, device)
    };
    let (surface_a, manager_a, device_a) = setup(&mut h, &mut a);
    let (surface_b, _manager_b, _device_b) = setup(&mut h, &mut b);

    let find = |h: &Harness, client: &Client, s: &wl_surface::WlSurface| {
        client
            .object_from_protocol_id::<SWlSurface>(&h.display.handle(), s.id().protocol_id())
            .expect("surface")
    };
    let server_a = find(&h, &id_a, &surface_a);
    let server_b = find(&h, &id_b, &surface_b);

    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    h.state
        .set_keyboard_focus(Some(FocusTarget::Surface(server_a)), serial);
    h.roundtrip(&mut a);

    let source = manager_a.create_source(&a.queue.handle(), ());
    source.offer("text/plain;charset=utf-8".into());
    device_a.set_selection(Some(&source), serial.into());
    h.roundtrip(&mut a);

    let before = b.probe.selections;
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    h.state
        .set_keyboard_focus(Some(FocusTarget::Surface(server_b)), serial);
    h.roundtrip(&mut b);

    assert!(
        b.probe.selections > before,
        "the newly focused client was never told the primary selection"
    );
    assert!(
        b.probe
            .offered
            .iter()
            .any(|mime| mime == "text/plain;charset=utf-8"),
        "the primary selection reached the focused client without its types: {:?}",
        b.probe.offered
    );
}

fn peer_on(h: &mut Harness, client: UnixStream) -> Peer {
    let conn = Connection::from_socket(client).expect("client connection");
    let queue = conn.new_event_queue();
    let registry = conn.display().get_registry(&queue.handle(), ());
    let mut peer = Peer {
        conn,
        queue,
        probe: Probe::default(),
        registry,
    };
    h.roundtrip(&mut peer);
    peer
}

#[test]
fn an_input_method_is_activated_for_the_focused_text_input() {
    let mut h = Harness::new();
    h.state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .expect("keyboard");
    let (server_t, client_t) = UnixStream::pair().expect("pair");
    let (server_i, client_i) = UnixStream::pair().expect("pair");
    let mut dh = h.display.handle();
    let app = dh
        .insert_client(server_t, Arc::new(ClientState::default()))
        .expect("app");
    dh.insert_client(server_i, Arc::new(ClientState::default()))
        .expect("ime");
    let mut t = peer_on(&mut h, client_t);
    let mut i = peer_on(&mut h, client_i);

    let seat_i = i.bind::<wl_seat::WlSeat>(7);
    let _ime =
        i.bind::<ZwpInputMethodManagerV2>(1)
            .get_input_method(&seat_i, &i.queue.handle(), ());
    h.roundtrip(&mut i);

    let qh = t.queue.handle();
    let surface = t
        .bind::<wl_compositor::WlCompositor>(4)
        .create_surface(&qh, ());
    let seat_t = t.bind::<wl_seat::WlSeat>(7);
    let text_input = t
        .bind::<ZwpTextInputManagerV3>(1)
        .get_text_input(&seat_t, &qh, ());
    h.roundtrip(&mut t);

    let server = app
        .object_from_protocol_id::<SWlSurface>(&h.display.handle(), surface.id().protocol_id())
        .expect("surface");
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    h.state
        .set_keyboard_focus(Some(FocusTarget::Surface(server)), serial);
    h.roundtrip(&mut t);
    assert!(
        t.probe.text_input_entered,
        "the text input never entered the focused surface"
    );

    text_input.enable();
    text_input.commit();
    h.roundtrip(&mut t);
    h.roundtrip(&mut i);
    assert!(
        i.probe.ime_activated,
        "the input method was never activated"
    );
    assert!(i.probe.ime_done);
}

#[test]
fn a_token_asked_for_without_input_is_untrusted() {
    let mut h = Harness::new();
    let mut peer = h.connect();
    let activation = peer.bind::<XdgActivationV1>(1);
    let token = activation.get_activation_token(&peer.queue.handle(), ());
    token.commit();
    h.roundtrip(&mut peer);
    let issued = peer.probe.token.clone().expect("no token was issued");
    assert_eq!(origin_of(&h.state, &issued), Some(Origin::Untrusted));
}

#[test]
fn a_launch_token_is_trusted() {
    let mut h = Harness::new();
    let token = h.state.launch_token();
    assert!(!token.is_empty());
    assert_eq!(origin_of(&h.state, &token), Some(Origin::Launch));
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            probe.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_device_v1::Event::Selection { .. } = event {
            probe.selections += 1;
        }
    }

    event_created_child!(Probe, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event else {
            return;
        };
        probe.offered.push(mime_type);
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            probe.token = Some(token);
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_text_input_v3::Event::Enter { .. } = event {
            probe.text_input_entered = true;
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => probe.ime_activated = true,
            zwp_input_method_v2::Event::Done => probe.ime_done = true,
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($ty:ty),*) => {$(
        impl Dispatch<$ty, ()> for Probe {
            fn event(_: &mut Self, _: &$ty, _: <$ty as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    wl_seat::WlSeat,
    ZwpPrimarySelectionDeviceManagerV1,
    ZwpPrimarySelectionSourceV1,
    ZwpTextInputManagerV3,
    ZwpInputMethodManagerV2,
    XdgActivationV1
);
