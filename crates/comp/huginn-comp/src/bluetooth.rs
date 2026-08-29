//! Bluetooth, through BlueZ on the system bus, for the quick settings row.
//!
//! # Shape
//!
//! The compositor does not speak D-Bus from its own loop. BlueZ's API wants a
//! connection with an executor behind it, and a `Pair()` can take the better
//! part of a minute while somebody finds the button on their headphones; a
//! frame loop that waits on either is a frame loop that has stopped. So the
//! whole of the D-Bus side lives on one thread, [`start`]ed alongside the
//! file indexer, and the loop sees three things: a [`State`] it can read
//! under a mutex, a [`Command`] channel it can drop requests into, and a
//! calloop wake-up for when the state changed and the panel should be
//! redrawn. The row in `settings.rs` reads the first and sends the second,
//! and never blocks on anything.
//!
//! # Pairing
//!
//! BlueZ refuses to pair for a caller that has no agent registered, because
//! pairing may need a question answered: "is 123456 the number on the
//! device?" for numeric comparison, or "type 123456 on the keyboard" for
//! passkey entry. The agent here is `DisplayYesNo`: it can show a number and
//! take a yes or a no, both of which the row can do, and it cannot type,
//! which the row cannot either. A question is surfaced as a [`Prompt`] in
//! the state, the agent's thread waits on the answer, and the row's Return
//! or a move off the row is what answers it. The agent lives only for the
//! duration of one pairing, on a connection of its own, so its wait never
//! sits in the way of polling.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use calloop::LoopHandle;
use calloop::channel::{self, Event};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::xwayland::AsHuginn;

const BLUEZ: &str = "org.bluez";
const AGENT_PATH: &str = "/org/raven/huginn/agent";
/// How often the thread re-reads BlueZ when nothing is going on. Devices
/// connect and disconnect by themselves — a trusted headset coming back into
/// range — and the row should notice within a moment of the panel opening.
const IDLE_POLL: Duration = Duration::from_secs(3);
/// How often while scanning or mid-operation, when things change by the second.
const BUSY_POLL: Duration = Duration::from_millis(400);
/// A scan nobody stopped stops itself. Discovery keeps the radio busy and
/// the panel may have been closed by something that never told the row.
const SCAN_FOR: Duration = Duration::from_secs(60);
/// How long the agent waits for a yes or no before telling BlueZ no.
const ANSWER_WITHIN: Duration = Duration::from_secs(60);

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// One device BlueZ knows about: paired, or seen during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Device {
    pub(crate) path: OwnedObjectPath,
    pub(crate) address: String,
    pub(crate) name: String,
    pub(crate) paired: bool,
    pub(crate) trusted: bool,
    pub(crate) connected: bool,
}

/// A question the pairing agent is waiting on, or a number it was told to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prompt {
    /// Numeric comparison: the same six digits are on the device. Yes or no.
    Confirm { device: String, passkey: u32 },
    /// Passkey entry on the other side: the person types these digits on the
    /// device (a keyboard). Nothing to answer; it clears when pairing ends.
    Display { device: String, passkey: u32 },
}

/// Everything the row shows, as last read from BlueZ.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    /// Whether `org.bluez` answered at all. False with no bluetoothd, or no
    /// adapter: the row is honest about being wired to nothing either way.
    pub(crate) available: bool,
    pub(crate) powered: bool,
    pub(crate) discovering: bool,
    /// Connected first, then paired, then merely seen; by name within each.
    pub(crate) devices: Vec<Device>,
    /// What the thread is doing right now, for the row to show in place of
    /// the reading: "Pairing Keyboard", "Connecting Headphones".
    pub(crate) busy: Option<String>,
    pub(crate) prompt: Option<Prompt>,
    /// The last operation's failure, until the row is left or acted on.
    pub(crate) error: Option<String>,
}

/// What the row can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Power(bool),
    Scan(bool),
    Connect(OwnedObjectPath),
    Disconnect(OwnedObjectPath),
    /// Pair, mark trusted so it reconnects on its own, then connect.
    Pair(OwnedObjectPath),
    ClearError,
}

/// What the row talks to. A trait for the same reason the Power row's
/// sender is one: the real thing needs a bus and a radio, and what the row
/// does with a prompt or a device list must be testable without either.
pub(crate) trait Backend: std::fmt::Debug {
    fn state(&self) -> State;
    fn send(&self, command: Command);
    /// Answer the open [`Prompt::Confirm`], if there is one.
    fn answer(&self, yes: bool);
}

/// The row's end of the BlueZ thread.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    shared: Arc<Mutex<State>>,
    commands: mpsc::Sender<Command>,
    answer: Arc<Answer>,
}

impl Backend for Client {
    fn state(&self) -> State {
        self.shared.lock().map(|s| s.clone()).unwrap_or_default()
    }
    fn send(&self, command: Command) {
        // A closed channel is a thread that died; the state it left says
        // "unavailable" and there is nothing more to do about it here.
        let _ = self.commands.send(command);
    }
    fn answer(&self, yes: bool) {
        self.answer.give(yes);
    }
}

/// Nothing behind the row: what it has until [`start`] gives it a
/// [`Client`], and what it keeps in tests and on a backend that never
/// started.
#[derive(Debug, Default)]
pub(crate) struct Unavailable;

impl Backend for Unavailable {
    fn state(&self) -> State {
        State::default()
    }
    fn send(&self, _: Command) {}
    fn answer(&self, _: bool) {}
}

/// The slot the agent's question is answered through.
#[derive(Debug, Default)]
struct Answer {
    slot: Mutex<Option<bool>>,
    changed: Condvar,
}

impl Answer {
    fn give(&self, yes: bool) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(yes);
            self.changed.notify_all();
        }
    }

    /// Clear a stale answer, so a Return pressed before the question was
    /// asked does not answer it.
    fn reset(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }

    fn wait(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let Ok(mut slot) = self.slot.lock() else {
            return false;
        };
        loop {
            if let Some(yes) = slot.take() {
                return yes;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.changed.wait_timeout(slot, left) {
                Ok((guard, _)) => slot = guard,
                Err(_) => return false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Starting
// ---------------------------------------------------------------------------

/// Start the BlueZ thread and hand the quick settings row its client.
///
/// Fail-soft like everything else that registers a source: without it the
/// row stays the "not connected" one it was, and the desktop is otherwise
/// exactly as before.
pub(crate) fn start<D>(handle: &LoopHandle<'static, D>, state: &mut crate::state::Huginn)
where
    D: AsHuginn + 'static,
{
    let (wake, woken) = channel::channel::<()>();
    let (commands, requests) = mpsc::channel::<Command>();
    let shared = Arc::new(Mutex::new(State::default()));
    let answer = Arc::new(Answer::default());

    let worker = Worker {
        shared: Arc::clone(&shared),
        answer: Arc::clone(&answer),
        wake,
        conn: None,
        scan_started: None,
    };
    let spawned = std::thread::Builder::new()
        .name("bluetooth".into())
        .spawn(move || worker.run(requests));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the Bluetooth thread");
        return;
    }

    let inserted = handle.insert_source(woken, |event, _, data: &mut D| {
        if let Event::Msg(()) = event {
            data.as_huginn().bluetooth_changed();
        }
    });
    if let Err(e) = inserted {
        tracing::warn!(error = %e, "could not register the Bluetooth thread");
        return;
    }

    state.set_bluetooth(Box::new(Client {
        shared,
        commands,
        answer,
    }));
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

struct Worker {
    shared: Arc<Mutex<State>>,
    answer: Arc<Answer>,
    wake: channel::Sender<()>,
    /// Made when first needed and remade if it goes away, so a bluetoothd
    /// that starts after the compositor is still found.
    conn: Option<Connection>,
    /// When the current scan began, for [`SCAN_FOR`].
    scan_started: Option<Instant>,
}

impl Worker {
    fn run(mut self, requests: mpsc::Receiver<Command>) {
        self.refresh();
        loop {
            let wait = if self.is_busy() { BUSY_POLL } else { IDLE_POLL };
            match requests.recv_timeout(wait) {
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
                // The client is gone; the compositor is on the way out.
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if let Some(started) = self.scan_started
                && started.elapsed() >= SCAN_FOR
                && let Err(e) = self.scan(false)
            {
                tracing::debug!(error = %e, "stopping a scan that ran its course");
            }
            self.refresh();
        }
    }

    fn is_busy(&self) -> bool {
        self.shared
            .lock()
            .map(|s| s.discovering || s.busy.is_some() || s.prompt.is_some())
            .unwrap_or(false)
    }

    fn connection(&mut self) -> Option<&Connection> {
        if self.conn.is_none() {
            self.conn = Connection::system().ok();
        }
        self.conn.as_ref()
    }

    /// Change the shared state and, if that changed anything, wake the loop.
    fn update(&self, change: impl FnOnce(&mut State)) {
        let Ok(mut state) = self.shared.lock() else {
            return;
        };
        let before = state.clone();
        change(&mut state);
        if *state != before {
            let _ = self.wake.send(());
        }
    }

    /// Re-read BlueZ into the state. The parts the thread owns — busy,
    /// prompt, error — are kept; the parts BlueZ owns are replaced.
    fn refresh(&mut self) {
        let read = self.connection().and_then(snapshot);
        if read.is_none() {
            // Drop a connection that stopped answering, so the next refresh
            // tries afresh rather than asking a dead socket forever.
            self.conn = None;
        }
        self.update(|state| match read {
            Some(fresh) => {
                state.available = true;
                state.powered = fresh.powered;
                state.discovering = fresh.discovering;
                state.devices = fresh.devices;
            }
            None => {
                state.available = false;
                state.powered = false;
                state.discovering = false;
                state.devices.clear();
            }
        });
        if !self.shared.lock().map(|s| s.discovering).unwrap_or(false) {
            self.scan_started = None;
        }
    }

    fn handle(&mut self, command: Command) {
        let result = match command {
            Command::Power(on) => self.power(on),
            Command::Scan(on) => self.scan(on),
            Command::Connect(path) => self.with_device(&path, "Connecting", |p, _| {
                p.call_method("Connect", &()).map(|_| ())
            }),
            Command::Disconnect(path) => self.with_device(&path, "Disconnecting", |p, _| {
                p.call_method("Disconnect", &()).map(|_| ())
            }),
            Command::Pair(path) => self.pair(&path),
            Command::ClearError => {
                self.update(|s| s.error = None);
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "bluetooth");
            self.update(|s| s.error = Some(e));
        }
    }

    fn adapter(&mut self) -> Result<(Connection, OwnedObjectPath), String> {
        let conn = self.connection().cloned().ok_or("no system bus")?;
        let snap = snapshot(&conn).ok_or("BlueZ is not running")?;
        let path = snap.adapter.ok_or("no Bluetooth adapter")?;
        Ok((conn, path))
    }

    fn power(&mut self, on: bool) -> Result<(), String> {
        let (conn, adapter) = self.adapter()?;
        Proxy::new(&conn, BLUEZ, adapter, "org.bluez.Adapter1")
            .and_then(|p| p.set_property("Powered", on).map_err(zbus::Error::from))
            .map_err(|e| short(&e))
    }

    fn scan(&mut self, on: bool) -> Result<(), String> {
        let (conn, adapter) = self.adapter()?;
        let p = Proxy::new(&conn, BLUEZ, adapter, "org.bluez.Adapter1").map_err(|e| short(&e))?;
        let already = self.shared.lock().map(|s| s.discovering).unwrap_or(false);
        if on && !already {
            // Everything, not just what advertises a service: a device in
            // pairing mode is a device that has never told us what it does.
            let filter: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
            let _ = p.call_method("SetDiscoveryFilter", &(filter,));
            p.call_method("StartDiscovery", &())
                .map_err(|e| short(&e))?;
            self.scan_started = Some(Instant::now());
        } else if !on && already {
            // Another program's scan is not ours to stop, and BlueZ says so
            // rather than stopping it; nothing to report.
            let _ = p.call_method("StopDiscovery", &());
            self.scan_started = None;
        }
        Ok(())
    }

    /// Run `op` on a device with the state marked busy as `verb` throughout.
    fn with_device(
        &mut self,
        path: &OwnedObjectPath,
        verb: &str,
        op: impl FnOnce(&Proxy<'_>, &Device) -> zbus::Result<()>,
    ) -> Result<(), String> {
        let conn = self.connection().cloned().ok_or("no system bus")?;
        let snap = snapshot(&conn).ok_or("BlueZ is not running")?;
        let device = snap
            .devices
            .iter()
            .find(|d| &d.path == path)
            .cloned()
            .ok_or("that device is gone")?;
        self.update(|s| {
            s.busy = Some(format!("{verb} {}", device.name));
            s.error = None;
        });
        let result = Proxy::new(&conn, BLUEZ, path.clone(), "org.bluez.Device1")
            .and_then(|p| op(&p, &device))
            .map_err(|e| short(&e));
        self.update(|s| s.busy = None);
        result
    }

    fn pair(&mut self, path: &OwnedObjectPath) -> Result<(), String> {
        // A scan left running slows pairing down and the list has done its
        // job: the device was just picked from it.
        let _ = self.scan(false);
        let answer = Arc::clone(&self.answer);
        let shared = Arc::clone(&self.shared);
        let wake = self.wake.clone();
        self.with_device(path, "Pairing", move |p, device| {
            if !device.paired {
                answer.reset();
                let agent = AgentGuard::register(Agent {
                    shared,
                    wake,
                    answer,
                })?;
                let paired = p.call_method("Pair", &()).map(|_| ());
                drop(agent);
                paired?;
            }
            if !device.trusted {
                p.set_property("Trusted", true)?;
            }
            p.call_method("Connect", &()).map(|_| ())
        })
    }
}

// ---------------------------------------------------------------------------
// Reading BlueZ
// ---------------------------------------------------------------------------

struct Snapshot {
    adapter: Option<OwnedObjectPath>,
    powered: bool,
    discovering: bool,
    devices: Vec<Device>,
}

fn get_bool(m: &HashMap<String, OwnedValue>, k: &str) -> bool {
    m.get(k)
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(false)
}

fn get_str(m: &HashMap<String, OwnedValue>, k: &str) -> String {
    m.get(k)
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

fn snapshot(conn: &Connection) -> Option<Snapshot> {
    let om = Proxy::new(conn, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").ok()?;
    let objects: Managed = om.call("GetManagedObjects", &()).ok()?;
    let mut snap = Snapshot {
        adapter: None,
        powered: false,
        discovering: false,
        devices: Vec::new(),
    };
    for (path, interfaces) in objects {
        if let Some(adapter) = interfaces.get("org.bluez.Adapter1")
            && snap.adapter.is_none()
        {
            snap.adapter = Some(path.clone());
            snap.powered = get_bool(adapter, "Powered");
            snap.discovering = get_bool(adapter, "Discovering");
        }
        if let Some(device) = interfaces.get("org.bluez.Device1") {
            let address = get_str(device, "Address");
            let alias = get_str(device, "Alias");
            let name = if alias.is_empty() {
                get_str(device, "Name")
            } else {
                alias
            };
            // BlueZ aliases a device it has only seen advertise by its
            // address with dashes. That is a name nobody can pick out of a
            // list, but it is also all there is until the name arrives.
            let name = if name.is_empty() {
                address.clone()
            } else {
                name
            };
            snap.devices.push(Device {
                path,
                address,
                name,
                paired: get_bool(device, "Paired"),
                trusted: get_bool(device, "Trusted"),
                connected: get_bool(device, "Connected"),
            });
        }
    }
    snap.adapter.as_ref()?;
    snap.devices.sort_by(|a, b| {
        (!a.connected, !a.paired, a.name.to_lowercase()).cmp(&(
            !b.connected,
            !b.paired,
            b.name.to_lowercase(),
        ))
    });
    Some(snap)
}

/// BlueZ's errors arrive as `org.bluez.Error.Failed: br-connection-profile-unavailable`.
/// The last word of the name and the message are what fits on a row.
fn short(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, message, _) => {
            let kind = name.as_str().rsplit('.').next().unwrap_or("Failed");
            match message {
                Some(m) if !m.is_empty() => format!("{kind}: {m}"),
                _ => kind.to_owned(),
            }
        }
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The agent
// ---------------------------------------------------------------------------

/// BlueZ's agent errors, spelled the way bluetoothd expects them.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    Rejected(String),
}

/// An `org.bluez.Agent1` for one pairing. See the module docs.
struct Agent {
    shared: Arc<Mutex<State>>,
    wake: channel::Sender<()>,
    answer: Arc<Answer>,
}

impl Agent {
    fn set_prompt(&self, prompt: Option<Prompt>) {
        if let Ok(mut state) = self.shared.lock()
            && state.prompt != prompt
        {
            state.prompt = prompt;
            let _ = self.wake.send(());
        }
    }

    fn name_of(&self, device: &OwnedObjectPath) -> String {
        self.shared
            .lock()
            .ok()
            .and_then(|s| {
                s.devices
                    .iter()
                    .find(|d| &d.path == device)
                    .map(|d| d.name.clone())
            })
            .unwrap_or_else(|| "device".to_owned())
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {
        self.set_prompt(None);
    }

    fn cancel(&self) {
        self.set_prompt(None);
        self.answer.give(false);
    }

    fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Rejected(
            "quick settings cannot type a PIN".into(),
        ))
    }

    fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        // A legacy PIN shown for typing on the device: same shape as a
        // passkey, and a four-digit PIN fits the six-digit slot.
        let passkey = pincode.parse().unwrap_or(0);
        let device = self.name_of(&device);
        self.set_prompt(Some(Prompt::Display { device, passkey }));
    }

    fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, AgentError> {
        Err(AgentError::Rejected(
            "quick settings cannot type a passkey".into(),
        ))
    }

    fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        let device = self.name_of(&device);
        self.set_prompt(Some(Prompt::Display { device, passkey }));
    }

    fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let device = self.name_of(&device);
        self.set_prompt(Some(Prompt::Confirm { device, passkey }));
        let yes = self.answer.wait(ANSWER_WITHIN);
        self.set_prompt(None);
        if yes {
            Ok(())
        } else {
            Err(AgentError::Rejected("not confirmed".into()))
        }
    }

    fn request_authorization(&self, _device: OwnedObjectPath) -> Result<(), AgentError> {
        // "Just works": the person picked this device from the list a
        // moment ago, and that was the authorization.
        Ok(())
    }

    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        Ok(())
    }
}

/// The agent, registered for as long as this is held.
struct AgentGuard {
    conn: Connection,
}

impl AgentGuard {
    fn register(agent: Agent) -> zbus::Result<Self> {
        // Its own connection: the agent's method handlers run on the
        // connection's executor, and a confirmation waits there for up to a
        // minute. On the polling connection that wait would be a frozen row.
        let conn = Connection::system()?;
        conn.object_server().at(AGENT_PATH, agent)?;
        let path = ObjectPath::try_from(AGENT_PATH)?;
        let manager = Proxy::new(&conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1")?;
        manager.call_method("RegisterAgent", &(&path, "DisplayYesNo"))?;
        Ok(Self { conn })
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        if let Ok(path) = ObjectPath::try_from(AGENT_PATH)
            && let Ok(manager) =
                Proxy::new(&self.conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1")
        {
            let _ = manager.call_method("UnregisterAgent", &(&path,));
        }
        let _ = self.conn.object_server().remove::<Agent, _>(AGENT_PATH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_given_before_the_wait_is_taken_by_it() {
        let answer = Answer::default();
        answer.give(true);
        assert!(answer.wait(Duration::from_millis(10)));
    }

    #[test]
    fn a_reset_answer_is_not_taken() {
        let answer = Answer::default();
        answer.give(true);
        answer.reset();
        assert!(!answer.wait(Duration::from_millis(10)));
    }

    #[test]
    fn waiting_ends_when_the_answer_arrives_from_another_thread() {
        let answer = Arc::new(Answer::default());
        let giver = Arc::clone(&answer);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            giver.give(true);
        });
        assert!(answer.wait(Duration::from_secs(5)));
        handle.join().unwrap();
    }

    #[test]
    fn short_keeps_the_kind_and_the_message() {
        let name = zbus::names::ErrorName::try_from("org.bluez.Error.Failed").unwrap();
        let e = zbus::Error::MethodError(
            name.into(),
            Some("br-connection-canceled".into()),
            zbus::message::Message::method_call("/", "X")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert_eq!(short(&e), "Failed: br-connection-canceled");
    }
}
