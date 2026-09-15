//! The `org.freedesktop.Notifications` server, on a thread of its own.
//!
//! # Shape
//!
//! The same as `bluetooth.rs`: the compositor never waits on D-Bus. zbus
//! answers method calls on its own executor, and each call is handed to the
//! loop as an [`Incoming`] through a sink the loop provides. The one thing a
//! call cannot wait for is the loop: `Notify` returns the new id at once, so
//! ids are handed out here, from [`Ids`], and the set of [`Open`] ids is kept
//! where both sides can see it so `CloseNotification` can refuse an id that
//! names nothing. Signals travel the other way, as [`Outgoing`] values on a
//! channel this thread drains.
//!
//! # The name
//!
//! What is new here is owning a well-known name. Huginn asks for it with
//! neither `DoNotQueue` nor `ReplaceExisting`. If another notification daemon
//! holds it, Huginn does not take it away: it waits in the bus's queue, and
//! the bus makes it the owner the moment that daemon exits. zbus's connection
//! builder always asks with `DoNotQueue`, which would turn a running daemon
//! into a failure to start, so the name is requested after the connection is
//! built instead.
//!
//! # The notification centre
//!
//! Beside the standard interface, the same object serves
//! `org.raven.Notifications`, for the desktop's own software — a bar's clock
//! panel — to show what is open and what recently closed, and to remove it.
//! The specification has no way to list notifications, and no application
//! needs one; this is not an API for applications. `List` answers from a
//! [`Listing`] the loop keeps current, so it never waits on the loop either,
//! and `Changed` is emitted whenever that listing does.
//!
//! # Failing soft
//!
//! No session bus, or a bus that goes away, is logged once and retried every
//! few seconds. The desktop runs without notifications rather than failing.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use enumflags2::BitFlags;
use huginn_core::notify::{Closed, Id, Ids, Invoked, Request};
use zbus::blocking::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::zvariant::{OwnedValue, Value};

pub(crate) const NAME: &str = "org.freedesktop.Notifications";
pub(crate) const PATH: &str = "/org/freedesktop/Notifications";

/// The notification centre's interface, on the same object as [`NAME`].
pub(crate) const CENTRE: &str = "org.raven.Notifications";

/// How long to wait before trying the bus again, and how often a live
/// connection is checked for having gone.
const RETRY: Duration = Duration::from_secs(5);

/// What Huginn tells clients it supports. Not `sound`, since it plays none,
/// and not `body-hyperlinks` until a link in a card can be clicked.
const CAPABILITIES: [&str; 5] = [
    "actions",
    "body",
    "body-markup",
    "icon-static",
    "persistence",
];

/// The version of the specification served.
const SPEC_VERSION: &str = "1.2";

/// Ids of the notifications that are open, shared by the bus thread and the
/// loop.
pub(crate) type Open = Arc<Mutex<HashSet<Id>>>;

/// What the notification centre lists, shared by the bus thread and the loop.
pub(crate) type Listing = Arc<Mutex<Snapshot>>;

/// The notification centre's list as the loop last published it.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// Newest first.
    pub(crate) entries: Vec<Entry>,
    /// The compositor's uptime when it was taken, and the moment that was:
    /// together they turn an uptime into a time of day.
    pub(crate) now: Duration,
    pub(crate) taken: Instant,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            now: Duration::ZERO,
            taken: Instant::now(),
        }
    }
}

/// One notification in the centre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) id: Id,
    pub(crate) app_name: String,
    pub(crate) app_icon: String,
    pub(crate) summary: String,
    /// Without its markup.
    pub(crate) body: String,
    /// In the compositor's uptime.
    pub(crate) arrived: Duration,
    /// Still open, rather than closed and kept in the history.
    pub(crate) open: bool,
}

/// An entry as `List` sends it: id, application, icon, summary, body, when it
/// arrived in Unix seconds, and whether it is still open.
type Listed = (u32, String, String, String, String, i64, bool);

impl Snapshot {
    fn listed(&self) -> Vec<Listed> {
        let wall = SystemTime::now();
        let since_taken = self.taken.elapsed();
        self.entries
            .iter()
            .map(|entry| {
                let age = self.now.saturating_sub(entry.arrived) + since_taken;
                let arrived = wall
                    .checked_sub(age)
                    .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |at| i64::try_from(at.as_secs()).unwrap_or(i64::MAX));
                (
                    entry.id,
                    entry.app_name.clone(),
                    entry.app_icon.clone(),
                    entry.summary.clone(),
                    entry.body.clone(),
                    arrived,
                    entry.open,
                )
            })
            .collect()
    }
}

/// A call for the loop.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notify {
        id: Id,
        request: Box<Request>,
        /// The caller's unique bus name, for telling applications apart in
        /// the log and, later, for finding the window a notification came
        /// from.
        sender: Option<String>,
    },
    Close(Id),
    /// The centre removed one: dismissed if open, and not kept in the history.
    Remove(Id),
    /// The centre removed everything.
    Clear,
}

/// Where calls go. Returns `false` once the loop is gone.
type Sink = Arc<dyn Fn(Incoming) -> bool + Send + Sync>;

struct Server {
    ids: Arc<Mutex<Ids>>,
    open: Open,
    sink: Sink,
}

// `NotificationClosed` and `ActionInvoked` are not declared here: they are
// emitted from outside any method call, by name, in `announce_until_closed`,
// and a declared signal nothing calls is dead code to the compiler.
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        #[zbus(header)] header: Header<'_>,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = self
            .ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .assign(replaces_id);
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
        let request = read_request(
            app_name,
            replaces_id,
            app_icon,
            summary,
            body,
            actions,
            &hints,
            expire_timeout,
        );
        let sender = header.sender().map(ToString::to_string);
        let incoming = Incoming::Notify {
            id,
            request: Box::new(request),
            sender,
        };
        if !(self.sink)(incoming) {
            tracing::debug!(id, "the compositor is gone; notification dropped");
        }
        id
    }

    fn close_notification(&self, id: u32) -> zbus::fdo::Result<()> {
        let open = self
            .open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&id);
        if !open {
            // The specification asks for an error when the id names nothing.
            return Err(zbus::fdo::Error::Failed(format!(
                "no open notification has id {id}"
            )));
        }
        (self.sink)(Incoming::Close(id));
        Ok(())
    }

    fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.map(String::from).to_vec()
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "Huginn".into(),
            "Raven".into(),
            env!("CARGO_PKG_VERSION").into(),
            SPEC_VERSION.into(),
        )
    }
}

/// `org.raven.Notifications`. See the module documentation.
struct Centre {
    listing: Listing,
    sink: Sink,
}

// `Changed` is emitted by name in `announce_until_closed`, like the standard
// interface's signals.
#[zbus::interface(name = "org.raven.Notifications")]
impl Centre {
    /// Open notifications and the history, newest first.
    fn list(&self) -> Vec<Listed> {
        self.listing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .listed()
    }

    /// Remove one notification, open or closed. An id that names nothing is
    /// not an error: it may have closed a moment ago.
    fn remove(&self, id: u32) {
        (self.sink)(Incoming::Remove(id));
    }

    /// Remove every notification, open or closed.
    fn clear(&self) {
        (self.sink)(Incoming::Clear);
    }
}

/// A signal for the loop's side to have sent.
#[derive(Debug)]
pub(crate) enum Outgoing {
    /// `NotificationClosed`.
    Closed(Closed),
    /// `ActionInvoked`.
    Invoked(Invoked),
    /// The centre's `Changed`: the [`Listing`] is different.
    Changed,
}

/// Serve until the loop goes away. Runs on the notifications thread.
pub(crate) fn serve<F>(
    sink: F,
    outgoing: &mpsc::Receiver<Outgoing>,
    open: &Open,
    listing: &Listing,
) where
    F: Fn(Incoming) -> bool + Send + Sync + 'static,
{
    let sink: Sink = Arc::new(sink);
    // Kept across reconnections, so an id is never handed out twice.
    let ids = Arc::new(Mutex::new(Ids::default()));
    let mut warned = false;
    loop {
        let server = Server {
            ids: Arc::clone(&ids),
            open: Arc::clone(open),
            sink: Arc::clone(&sink),
        };
        let centre = Centre {
            listing: Arc::clone(listing),
            sink: Arc::clone(&sink),
        };
        match connect(server, centre) {
            Ok(connection) => {
                warned = false;
                if announce_until_closed(&connection, outgoing) == Ended::CompositorGone {
                    return;
                }
                tracing::warn!("lost the session bus; notifications resume when it is back");
            }
            Err(e) if !warned => {
                tracing::warn!(error = %e, "could not serve notifications; retrying");
                warned = true;
            }
            Err(e) => tracing::debug!(error = %e, "still cannot serve notifications"),
        }
        // A signal while there is no bus has nobody to hear it, so it is
        // dropped; the wait still ends early if the compositor goes.
        let until = Instant::now() + RETRY;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if let Err(RecvTimeoutError::Disconnected) = outgoing.recv_timeout(left) {
                return;
            }
        }
    }
}

/// The flags the name is requested with: none.
///
/// Spelled out because the obvious spelling is wrong. `BitFlags::default()`
/// for this type is `AllowReplacement | ReplaceExisting | DoNotQueue`, which
/// asks the bus to take the name from its current owner, or to fail if that
/// owner will not give it up. Huginn waits its turn instead.
fn name_flags() -> BitFlags<RequestNameFlags> {
    BitFlags::empty()
}

fn connect(server: Server, centre: Centre) -> zbus::Result<Connection> {
    let connection = zbus::blocking::connection::Builder::session()?
        .serve_at(PATH, server)?
        .serve_at(PATH, centre)?
        .build()?;
    // Neither `DoNotQueue` nor `ReplaceExisting`. See the module documentation
    // and `name_flags`.
    match connection.request_name_with_flags(NAME, name_flags())? {
        RequestNameReply::InQueue => tracing::info!(
            "another notification daemon holds {NAME}; Huginn takes over when it exits"
        ),
        reply => tracing::info!(?reply, "serving notifications"),
    }
    Ok(connection)
}

#[derive(Debug, PartialEq, Eq)]
enum Ended {
    BusLost,
    CompositorGone,
}

/// Emit every signal the loop reports, until the connection drops or the loop
/// does.
fn announce_until_closed(connection: &Connection, outgoing: &mpsc::Receiver<Outgoing>) -> Ended {
    loop {
        match outgoing.recv_timeout(RETRY) {
            Ok(Outgoing::Closed(Closed { id, reason })) => {
                let sent = connection.emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "NotificationClosed",
                    &(id, reason.code()),
                );
                if let Err(e) = sent {
                    tracing::debug!(error = %e, id, "could not announce a closed notification");
                }
            }
            Ok(Outgoing::Invoked(Invoked { id, key })) => {
                let sent = connection.emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "ActionInvoked",
                    &(id, key.as_str()),
                );
                if let Err(e) = sent {
                    tracing::debug!(error = %e, id, "could not announce an action");
                }
            }
            Ok(Outgoing::Changed) => {
                let sent = connection.emit_signal(None::<&str>, PATH, CENTRE, "Changed", &());
                if let Err(e) = sent {
                    tracing::debug!(error = %e, "could not announce a changed list");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ended::CompositorGone,
        }
        if connection.is_closed() {
            return Ended::BusLost;
        }
    }
}

/// Turn `Notify`'s arguments into a [`Request`], reading the hints the core
/// uses. Every other hint is ignored, as the specification allows.
#[allow(clippy::too_many_arguments)]
fn read_request(
    app_name: String,
    replaces_id: Id,
    app_icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: &HashMap<String, OwnedValue>,
    expire_timeout: i32,
) -> Request {
    let hint = |key: &str| hints.get(key).map(|value| &**value);
    Request {
        app_name,
        replaces_id,
        app_icon,
        summary,
        body,
        actions,
        expire_timeout,
        urgency: hint("urgency").and_then(as_byte),
        category: hint("category").and_then(as_text),
        desktop_entry: hint("desktop-entry").and_then(as_text),
        transient: hint("transient").and_then(as_flag).unwrap_or(false),
        resident: hint("resident").and_then(as_flag).unwrap_or(false),
    }
}

/// A hint that should be a byte. Some clients send another integer type, and
/// a number in range is taken as meant.
fn as_byte(value: &Value<'_>) -> Option<u8> {
    match value {
        Value::U8(n) => Some(*n),
        Value::I16(n) => u8::try_from(*n).ok(),
        Value::U16(n) => u8::try_from(*n).ok(),
        Value::I32(n) => u8::try_from(*n).ok(),
        Value::U32(n) => u8::try_from(*n).ok(),
        Value::I64(n) => u8::try_from(*n).ok(),
        Value::U64(n) => u8::try_from(*n).ok(),
        Value::Value(inner) => as_byte(inner),
        _ => None,
    }
}

/// A hint that should be a string. An empty one says nothing.
fn as_text(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(text) => Some(text.to_string()).filter(|t| !t.is_empty()),
        Value::Value(inner) => as_text(inner),
        _ => None,
    }
}

/// A hint that should be a boolean. Some clients send 0 or 1.
fn as_flag(value: &Value<'_>) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::U8(n) => Some(*n != 0),
        Value::I32(n) => Some(*n != 0),
        Value::U32(n) => Some(*n != 0),
        Value::Value(inner) => as_flag(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use huginn_core::notify::CloseReason;
    use std::os::unix::net::UnixStream;

    fn owned(value: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(value).expect("no file descriptors in these values")
    }

    fn request_with(hints: &HashMap<String, OwnedValue>) -> Request {
        read_request(
            "app".into(),
            0,
            String::new(),
            "summary".into(),
            String::new(),
            Vec::new(),
            hints,
            -1,
        )
    }

    #[test]
    fn the_name_is_requested_without_taking_it_or_refusing_to_wait() {
        assert!(
            name_flags().is_empty(),
            "no ReplaceExisting, no DoNotQueue, no AllowReplacement"
        );
        assert!(
            !BitFlags::<RequestNameFlags>::default().is_empty(),
            "the default is not empty, which is why name_flags exists"
        );
    }

    #[test]
    fn hints_are_read_in_the_types_clients_actually_send() {
        let hints = HashMap::from([
            ("urgency".to_string(), owned(Value::I32(2))),
            ("category".to_string(), owned(Value::from("im.received"))),
            (
                "desktop-entry".to_string(),
                owned(Value::from("com.ravenoracle.Raven")),
            ),
            ("transient".to_string(), owned(Value::U32(1))),
            (
                "resident".to_string(),
                owned(Value::Value(Box::new(Value::Bool(true)))),
            ),
            ("sound-name".to_string(), owned(Value::from("bell"))),
        ]);
        let request = request_with(&hints);
        assert_eq!(request.urgency, Some(2));
        assert_eq!(request.category.as_deref(), Some("im.received"));
        assert_eq!(
            request.desktop_entry.as_deref(),
            Some("com.ravenoracle.Raven")
        );
        assert!(request.transient);
        assert!(
            request.resident,
            "a boolean wrapped in a variant is still read"
        );
    }

    #[test]
    fn missing_or_nonsense_hints_leave_the_defaults() {
        let hints = HashMap::from([
            ("urgency".to_string(), owned(Value::I32(900))),
            ("desktop-entry".to_string(), owned(Value::from(""))),
            ("transient".to_string(), owned(Value::from("yes"))),
        ]);
        let request = request_with(&hints);
        assert_eq!(request.urgency, None, "out of range is not a byte");
        assert_eq!(request.desktop_entry, None, "empty says nothing");
        assert!(!request.transient);
        assert!(!request.resident);
        assert_eq!(request.expire_timeout, -1);
    }

    /// The server on one end of a socket pair and a client on the other: the
    /// real interface, real marshalling and real signals, with no bus.
    #[test]
    fn a_client_gets_ids_answers_and_closed_signals_over_a_real_connection() {
        let (done, finished) = mpsc::channel();
        // Run the whole exchange on a thread, so a signal that never arrives
        // fails the test instead of hanging the run.
        std::thread::spawn(move || {
            exchange();
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(20))
            .expect("the exchange finished");
    }

    /// On a real bus, the name: Huginn waits behind a daemon that holds it and
    /// takes over when that daemon lets go. Never run against the session's
    /// own bus, only a throwaway one:
    ///
    /// ```text
    /// dbus-run-session -- env HUGINN_TEST_PRIVATE_BUS=1 \
    ///     cargo test -p huginn-comp private_bus -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a private session bus; see the comment above"]
    fn on_a_private_bus_huginn_queues_behind_another_daemon_and_takes_over() {
        if std::env::var_os("HUGINN_TEST_PRIVATE_BUS").is_none() {
            eprintln!("skipped: HUGINN_TEST_PRIVATE_BUS is not set, so this may be a real session");
            return;
        }
        // The server reports trouble through tracing; show it when this fails.
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let squatter = Connection::session().unwrap();
        // An owner that even allows replacement: the case where asking with
        // `ReplaceExisting` would take the name, and Huginn must not.
        squatter
            .request_name_with_flags(NAME, RequestNameFlags::AllowReplacement.into())
            .unwrap();

        let (to_loop, from_bus) = mpsc::channel::<Incoming>();
        let (report, reports) = mpsc::channel::<Outgoing>();
        let open: Open = Arc::default();
        let serving_open = Arc::clone(&open);
        let serving = std::thread::spawn(move || {
            serve(
                move |call| to_loop.send(call).is_ok(),
                &reports,
                &serving_open,
                &Listing::default(),
            );
        });

        let client = Connection::session().unwrap();
        let queued = || -> Vec<String> {
            client
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "ListQueuedOwners",
                    &(NAME,),
                )
                .and_then(|reply| reply.body().deserialize::<Vec<String>>())
                .unwrap_or_else(|e| {
                    eprintln!("ListQueuedOwners failed: {e}");
                    Vec::new()
                })
        };

        wait_for("Huginn to join the queue for the name", || {
            queued().len() == 2
        });
        assert_eq!(
            queued()[0],
            squatter.unique_name().unwrap().as_str(),
            "the daemon that was there first keeps the name"
        );

        squatter.release_name(NAME).unwrap();
        wait_for("Huginn to become the owner", || queued().len() == 1);

        let proxy = zbus::blocking::Proxy::new(&client, NAME, PATH, NAME).unwrap();
        let (server, ..): (String, String, String, String) =
            proxy.call("GetServerInformation", &()).unwrap();
        assert_eq!(server, "Huginn");
        let id: u32 = proxy
            .call(
                "Notify",
                &(
                    "test",
                    0u32,
                    "",
                    "hello",
                    "",
                    Vec::<&str>::new(),
                    HashMap::<&str, Value<'_>>::new(),
                    -1i32,
                ),
            )
            .unwrap();
        let arrived = from_bus.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(arrived, Incoming::Notify { id: got, .. } if got == id));

        // The loop going away ends the thread.
        drop(report);
        serving.join().unwrap();
    }

    fn wait_for(what: &str, condition: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // `unix_stream` is deprecated only for builds with zbus's `tokio` feature,
    // which this workspace does not enable; the replacement wants an async-io
    // stream type the compositor has no other use for.
    #[allow(deprecated)]
    fn exchange() {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let (to_loop, from_bus) = mpsc::channel::<Incoming>();
        let open: Open = Arc::default();
        let sink: Sink = Arc::new(move |incoming| to_loop.send(incoming).is_ok());
        let server = Server {
            ids: Arc::default(),
            open: Arc::clone(&open),
            sink: Arc::clone(&sink),
        };
        let listing: Listing = Arc::default();
        let centre = Centre {
            listing: Arc::clone(&listing),
            sink,
        };
        let guid = zbus::Guid::generate();
        let serving = std::thread::spawn(move || {
            zbus::blocking::connection::Builder::unix_stream(server_end)
                .server(guid)
                .unwrap()
                .p2p()
                .serve_at(PATH, server)
                .unwrap()
                .serve_at(PATH, centre)
                .unwrap()
                .build()
                .unwrap()
        });
        let client = zbus::blocking::connection::Builder::unix_stream(client_end)
            .p2p()
            .build()
            .unwrap();
        let server_connection = serving.join().unwrap();
        let proxy = zbus::blocking::Proxy::new(&client, NAME, PATH, NAME).unwrap();

        // Notify: an id, the call handed to the loop, the hints read.
        let hints = HashMap::from([("urgency", Value::from(2u8))]);
        let id: u32 = proxy
            .call(
                "Notify",
                &(
                    "Oracle",
                    0u32,
                    "",
                    "Answer ready",
                    "<b>done</b>",
                    vec!["default", "Open"],
                    hints,
                    -1i32,
                ),
            )
            .unwrap();
        assert_eq!(id, 1);
        match from_bus.recv().unwrap() {
            Incoming::Notify { id, request, .. } => {
                assert_eq!(id, 1);
                assert_eq!(request.urgency, Some(2));
                assert_eq!(request.body, "<b>done</b>");
                assert_eq!(request.actions, ["default", "Open"]);
            }
            other => panic!("expected a notification, got {other:?}"),
        }

        // A replacement gets the id it named back.
        let replaced: u32 = proxy
            .call(
                "Notify",
                &(
                    "Oracle",
                    1u32,
                    "",
                    "again",
                    "",
                    Vec::<&str>::new(),
                    HashMap::<&str, Value<'_>>::new(),
                    0i32,
                ),
            )
            .unwrap();
        assert_eq!(replaced, 1);
        assert!(matches!(
            from_bus.recv().unwrap(),
            Incoming::Notify { id: 1, .. }
        ));

        let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).unwrap();
        assert!(capabilities.iter().any(|c| c == "actions"));
        assert!(capabilities.iter().any(|c| c == "body-markup"));
        let (name, vendor, _version, spec): (String, String, String, String) =
            proxy.call("GetServerInformation", &()).unwrap();
        assert_eq!(
            (name.as_str(), vendor.as_str(), spec.as_str()),
            ("Huginn", "Raven", "1.2")
        );

        // CloseNotification: passed on for an open id, refused for anything else.
        let () = proxy.call("CloseNotification", &(1u32,)).unwrap();
        assert!(matches!(from_bus.recv().unwrap(), Incoming::Close(1)));
        let refused: zbus::Result<()> = proxy.call("CloseNotification", &(99u32,));
        assert!(refused.is_err(), "an id that names nothing is an error");

        // The centre: List answers from the listing, Remove and Clear go to
        // the loop.
        let centre = zbus::blocking::Proxy::new(&client, NAME, PATH, CENTRE).unwrap();
        listing.lock().unwrap().entries = vec![Entry {
            id: 1,
            app_name: "Oracle".into(),
            app_icon: String::new(),
            summary: "Answer ready".into(),
            body: "done".into(),
            arrived: Duration::ZERO,
            open: false,
        }];
        let listed: Vec<Listed> = centre.call("List", &()).unwrap();
        assert_eq!(listed.len(), 1);
        let (id, app, _, summary, body, arrived, open) = &listed[0];
        assert_eq!(
            (*id, app.as_str(), summary.as_str(), body.as_str(), *open),
            (1, "Oracle", "Answer ready", "done", false)
        );
        assert!(*arrived > 1_600_000_000, "a time of day, in Unix seconds");
        let () = centre.call("Remove", &(1u32,)).unwrap();
        assert!(matches!(from_bus.recv().unwrap(), Incoming::Remove(1)));
        let () = centre.call("Clear", &()).unwrap();
        assert!(matches!(from_bus.recv().unwrap(), Incoming::Clear));
        let mut changed = centre.receive_signal("Changed").unwrap();

        // ActionInvoked and NotificationClosed: what the loop reports is what
        // the client hears.
        let mut invoked = proxy.receive_signal("ActionInvoked").unwrap();
        let mut closed = proxy.receive_signal("NotificationClosed").unwrap();
        let (report, reports) = mpsc::channel();
        let announcing =
            std::thread::spawn(move || announce_until_closed(&server_connection, &reports));
        report
            .send(Outgoing::Invoked(Invoked {
                id: 1,
                key: "reply".into(),
            }))
            .unwrap();
        report
            .send(Outgoing::Closed(Closed {
                id: 1,
                reason: CloseReason::Dismissed,
            }))
            .unwrap();
        let message = invoked.next().expect("an ActionInvoked signal");
        let (invoked_id, key): (u32, String) = message.body().deserialize().unwrap();
        assert_eq!((invoked_id, key.as_str()), (1, "reply"));
        let message = closed.next().expect("a NotificationClosed signal");
        let (closed_id, reason): (u32, u32) = message.body().deserialize().unwrap();
        assert_eq!((closed_id, reason), (1, 2));
        report.send(Outgoing::Changed).unwrap();
        changed.next().expect("a Changed signal");

        drop(report);
        assert_eq!(announcing.join().unwrap(), Ended::CompositorGone);
    }
}
