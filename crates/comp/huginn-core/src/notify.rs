//! Notifications: what arrives, where it goes, and when it leaves.
//!
//! Huginn serves `org.freedesktop.Notifications` itself rather than leaving it
//! to a daemon (see `docs/notifications.md`). Everything that decides anything
//! about a notification lives here: how the specification's arguments are
//! read, which notifications interrupt and which wait quietly, how long a card
//! stays, and which close reason a client is told. The D-Bus thread turns a
//! call into a [`Request`], the compositor hands the result to a [`Queue`]
//! along with the desktop's [`Context`], draws whatever [`Queue::visible`]
//! returns, and sends the [`Closed`] and [`Invoked`] values it gets back as
//! signals.
//!
//! Nothing here reads the clock. Every method that cares about time takes
//! `now`, the compositor's uptime, so a card's six seconds on screen are
//! tested in microseconds.
//!
//! ```
//! use std::time::Duration;
//! use huginn_core::notify::{CloseReason, Ids, Notification, Queue, Request};
//!
//! let mut ids = Ids::default();
//! let mut queue = Queue::default();
//!
//! let request = Request { summary: "Answer ready".into(), ..Request::default() };
//! let id = ids.assign(request.replaces_id);
//! queue.notify(Notification::from_request(id, request, Duration::ZERO), Duration::ZERO);
//! assert_eq!(queue.visible().count(), 1);
//!
//! // A normal notification with the default timeout is gone after six seconds.
//! let closed = queue.tick(Duration::from_secs(6));
//! assert_eq!(closed[0].reason, CloseReason::Expired);
//! ```

use std::collections::VecDeque;
use std::time::Duration;

/// A notification's id, as the specification defines it. Zero never names a
/// notification: it is what a client sends when it is not replacing one.
pub type Id = u32;

/// Cards on screen at once. More wait out of view, and the stack says how
/// many.
pub const MAX_CARDS: usize = 3;

/// Silent notifications kept open in the tray, and notifications waiting for
/// the session to unlock. The oldest is closed beyond this, so a client that
/// notifies in a loop cannot grow the compositor without bound.
pub const TRAY_LIMIT: usize = 50;

/// Closed notifications remembered for the session.
pub const HISTORY_LIMIT: usize = 50;

/// The action key the specification reserves for clicking the notification
/// itself.
pub const DEFAULT_ACTION: &str = "default";

/// Hands out notification ids.
///
/// Lives on the D-Bus thread, because `Notify` has to return the id before the
/// compositor has seen the notification.
#[derive(Debug, Clone, Default)]
pub struct Ids {
    last: Id,
}

impl Ids {
    /// The id for a `Notify` call.
    ///
    /// A client that names a notification to replace gets that id back, as
    /// the specification requires, and later ids skip past it so they cannot
    /// collide with it. Otherwise ids count up from one. After four billion
    /// they wrap back to one, skipping zero.
    pub fn assign(&mut self, replaces: Id) -> Id {
        if replaces != 0 {
            self.last = self.last.max(replaces);
            return replaces;
        }
        self.last = self.last.checked_add(1).unwrap_or(1);
        self.last
    }
}

/// How much a notification should interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    /// The `urgency` hint's byte. The specification defines 0, 1 and 2;
    /// anything else is read as normal rather than refused.
    pub const fn from_hint(byte: u8) -> Self {
        match byte {
            0 => Self::Low,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }
}

/// What a client asked for with `expire_timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expire {
    /// `-1`: the server decides. See [`Timeouts`].
    Default,
    /// `0`: stays until dismissed.
    Never,
    /// A number of milliseconds.
    After(Duration),
}

impl Expire {
    pub fn from_timeout(milliseconds: i32) -> Self {
        match milliseconds {
            ms if ms < 0 => Self::Default,
            0 => Self::Never,
            ms => Self::After(Duration::from_millis(u64::from(ms.unsigned_abs()))),
        }
    }
}

/// How long a card stays when its client left the choice to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub low: Duration,
    pub normal: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            low: Duration::from_secs(4),
            normal: Duration::from_secs(6),
        }
    }
}

impl Timeouts {
    /// A card's time on screen, or `None` for one that stays until dismissed.
    ///
    /// A critical notification does not expire on its own — the specification
    /// asks for that, and something critical going unseen because it timed
    /// out is the failure it exists to prevent. A client that sets an explicit
    /// timeout on one still gets it.
    pub const fn lifetime(self, expire: Expire, urgency: Urgency) -> Option<Duration> {
        match (expire, urgency) {
            (Expire::Never, _) | (Expire::Default, Urgency::Critical) => None,
            (Expire::After(duration), _) => Some(duration),
            (Expire::Default, Urgency::Low) => Some(self.low),
            (Expire::Default, Urgency::Normal) => Some(self.normal),
        }
    }
}

/// Why a notification closed, as `NotificationClosed` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// Its time ran out.
    Expired,
    /// The person dismissed it, or acted on it.
    Dismissed,
    /// Its client called `CloseNotification`.
    Closed,
    /// Anything else: here, pushed out of a full tray.
    Undefined,
}

impl CloseReason {
    /// The number the specification gives each reason.
    pub const fn code(self) -> u32 {
        match self {
            Self::Expired => 1,
            Self::Dismissed => 2,
            Self::Closed => 3,
            Self::Undefined => 4,
        }
    }
}

/// A `NotificationClosed` signal to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed {
    pub id: Id,
    pub reason: CloseReason,
}

/// An `ActionInvoked` signal to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoked {
    pub id: Id,
    pub key: String,
}

/// A button a client offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub key: String,
    pub label: String,
}

impl Action {
    /// The specification's flat list, `[key, label, key, label, …]`.
    ///
    /// A trailing key with no label is dropped: a button with no words on it
    /// is not one anybody can use.
    pub fn from_pairs(flat: &[String]) -> Vec<Action> {
        let (pairs, _dangling) = flat.as_chunks::<2>();
        pairs
            .iter()
            .map(|[key, label]| Action {
                key: key.clone(),
                label: label.clone(),
            })
            .collect()
    }
}

/// A run of body text in one style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// The target of the `<a href>` this run is inside.
    pub link: Option<String>,
}

/// Read a body in the specification's markup.
///
/// The specification allows `<b>`, `<i>`, `<u>`, `<a href>` and `<img>`.
/// Those are honoured; `<br>` becomes a line break, an image becomes its alt
/// text, and any other tag is stripped with its text kept. The named entities
/// XML defines, `&nbsp;` and numeric references are decoded; anything else
/// that looks like one is left as written.
///
/// Clients that claim to send markup often do not. A `<` that does not begin
/// a tag — `1 < 2`, `<3` — stays text, and nothing here ever fails: the worst
/// a malformed body can do is look slightly wrong.
pub fn parse_markup(source: &str) -> Vec<Span> {
    let mut parser = Parser::default();
    let mut rest = source;
    while let Some(at) = rest.find(['<', '&']) {
        parser.text(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('&') {
            match entity(rest) {
                Some((decoded, length)) => {
                    parser.text(decoded.encode_utf8(&mut [0; 4]));
                    rest = &rest[length..];
                }
                None => {
                    parser.text("&");
                    rest = &rest[1..];
                }
            }
        } else if let Some((tag, length)) = tag(rest) {
            parser.apply(&tag);
            rest = &rest[length..];
        } else {
            parser.text("<");
            rest = &rest[1..];
        }
    }
    parser.text(rest);
    parser.spans
}

/// The words of a body without its styling, for a preview or the history.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// The markup parser's state: the spans so far and the styles open now.
///
/// Styles are counted rather than flagged, so `<b><b>x</b>y</b>` keeps `y`
/// bold, and a stray closing tag cannot count below zero.
#[derive(Debug, Default)]
struct Parser {
    spans: Vec<Span>,
    bold: u32,
    italic: u32,
    underline: u32,
    link: Option<String>,
}

impl Parser {
    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let (bold, italic, underline) = (self.bold > 0, self.italic > 0, self.underline > 0);
        if let Some(last) = self.spans.last_mut()
            && last.bold == bold
            && last.italic == italic
            && last.underline == underline
            && last.link == self.link
        {
            last.text.push_str(text);
            return;
        }
        self.spans.push(Span {
            text: text.to_string(),
            bold,
            italic,
            underline,
            link: self.link.clone(),
        });
    }

    fn apply(&mut self, tag: &Tag<'_>) {
        match (tag.name.as_str(), tag.closing) {
            ("b", false) => self.bold += 1,
            ("b", true) => self.bold = self.bold.saturating_sub(1),
            ("i", false) => self.italic += 1,
            ("i", true) => self.italic = self.italic.saturating_sub(1),
            ("u", false) => self.underline += 1,
            ("u", true) => self.underline = self.underline.saturating_sub(1),
            ("a", false) => self.link = attribute(tag.attributes, "href"),
            ("a", true) => self.link = None,
            ("img", false) => {
                if let Some(alt) = attribute(tag.attributes, "alt") {
                    self.text(&alt);
                }
            }
            ("br", _) => self.text("\n"),
            _ => {}
        }
    }
}

#[derive(Debug)]
struct Tag<'a> {
    /// Lowercase: `<B>` is `<b>`.
    name: String,
    closing: bool,
    attributes: &'a str,
}

/// The tag at the start of `source`, which begins with `<`, and its length.
///
/// `None` when it is not one: no closing `>`, another `<` before it, or a name
/// that does not start with a letter. Those are text.
fn tag(source: &str) -> Option<(Tag<'_>, usize)> {
    let end = source.find('>')?;
    let inner = &source[1..end];
    if inner.contains('<') {
        return None;
    }
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    if !inner.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let name_end = inner
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(inner.len());
    let (name, attributes) = inner.split_at(name_end);
    if !attributes.is_empty() && !attributes.starts_with(char::is_whitespace) {
        return None;
    }
    Some((
        Tag {
            name: name.to_ascii_lowercase(),
            closing,
            attributes,
        },
        end + 1,
    ))
}

/// The value of attribute `wanted`, quoted or not, with entities decoded.
fn attribute(attributes: &str, wanted: &str) -> Option<String> {
    let mut rest = attributes.trim_start();
    while !rest.is_empty() {
        let key_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        if key_end == 0 {
            // A stray `=`, the only thing that can start a trimmed remainder
            // without being a key.
            rest = rest[1..].trim_start();
            continue;
        }
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        let Some(after) = rest.strip_prefix('=') else {
            // A bare attribute, such as `<input disabled>`.
            continue;
        };
        let after = after.trim_start();
        let (value, remainder) = match after.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let body = &after[1..];
                match body.find(quote) {
                    Some(close) => (&body[..close], &body[close + 1..]),
                    None => (body, ""),
                }
            }
            _ => {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                after.split_at(end)
            }
        };
        if key.eq_ignore_ascii_case(wanted) {
            return Some(decode_entities(value));
        }
        rest = remainder.trim_start();
    }
    None
}

fn decode_entities(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match entity(rest) {
            Some((decoded, length)) => {
                out.push(decoded);
                rest = &rest[length..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The entity at the start of `source`, which begins with `&`, and its length.
fn entity(source: &str) -> Option<(char, usize)> {
    // Counted in characters, not bytes, so a multibyte character after the
    // `&` cannot put the search in the middle of one.
    let end = source
        .char_indices()
        .take(12)
        .find(|&(_, c)| c == ';')
        .map(|(i, _)| i)?;
    let name = &source[1..end];
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((decoded, end + 1))
}

/// A `Notify` call, as the D-Bus side reads it.
///
/// Hints the core has no use for — images, sounds — are the compositor's to
/// keep beside the notification, keyed by its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub app_name: String,
    pub replaces_id: Id,
    pub app_icon: String,
    /// Plain text: the specification allows markup only in the body.
    pub summary: String,
    pub body: String,
    /// The flat `[key, label, …]` list.
    pub actions: Vec<String>,
    pub expire_timeout: i32,
    /// The `urgency` hint, when there was one.
    pub urgency: Option<u8>,
    pub category: Option<String>,
    pub desktop_entry: Option<String>,
    pub transient: bool,
    pub resident: bool,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            app_name: String::new(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: String::new(),
            body: String::new(),
            actions: Vec::new(),
            // What a client means when it expresses no opinion.
            expire_timeout: -1,
            urgency: None,
            category: None,
            desktop_entry: None,
            transient: false,
            resident: false,
        }
    }
}

/// A notification, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: Id,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: Vec<Span>,
    pub actions: Vec<Action>,
    pub urgency: Urgency,
    pub category: Option<String>,
    pub desktop_entry: Option<String>,
    /// Not to be kept once it has gone: no tray, no history.
    pub transient: bool,
    /// Not closed by invoking one of its actions.
    pub resident: bool,
    pub expire: Expire,
    /// When it arrived, in the compositor's uptime.
    pub arrived: Duration,
}

impl Notification {
    pub fn from_request(id: Id, request: Request, now: Duration) -> Self {
        Self {
            id,
            app_name: request.app_name,
            app_icon: request.app_icon,
            summary: request.summary,
            body: parse_markup(&request.body),
            actions: Action::from_pairs(&request.actions),
            urgency: request.urgency.map_or(Urgency::Normal, Urgency::from_hint),
            category: request.category,
            desktop_entry: request.desktop_entry,
            transient: request.transient,
            resident: request.resident,
            expire: Expire::from_timeout(request.expire_timeout),
            arrived: now,
        }
    }

    /// The action a click on the card invokes, if the client offered one.
    pub fn default_action(&self) -> Option<&Action> {
        self.actions.iter().find(|a| a.key == DEFAULT_ACTION)
    }

    /// The actions drawn as buttons: every one but the default, which is the
    /// card itself.
    pub fn buttons(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| a.key != DEFAULT_ACTION)
    }
}

/// What the desktop is doing, as far as notifications care.
///
/// Screen recording is not here: a recording changes what is captured, not
/// what is shown, and the compositor leaves the cards out of the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Context {
    /// Nothing is shown and no time passes until unlock.
    pub locked: bool,
    pub do_not_disturb: bool,
    /// A fullscreen window on the active workspace.
    pub fullscreen: bool,
    /// A surface on screen is holding off idle: a video, a presentation.
    pub idle_inhibited: bool,
}

/// Whether a notification interrupts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    /// A card on screen.
    Show,
    /// Kept open in the tray, without a card.
    Silent,
}

/// The policy: whether a notification of `urgency` interrupts now.
///
/// Do not disturb, a fullscreen window and an idle inhibitor all mean the
/// person is busy with something they chose, and only critical notifications
/// get through. Nothing is lost by being silent: it waits in the tray.
pub const fn present(urgency: Urgency, context: Context) -> Presentation {
    let busy = context.do_not_disturb || context.fullscreen || context.idle_inhibited;
    match urgency {
        Urgency::Critical => Presentation::Show,
        _ if busy => Presentation::Silent,
        _ => Presentation::Show,
    }
}

/// Where a notification went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Card,
    Tray,
    /// Held until the session unlocks.
    Waiting,
    /// Silent and transient: closed at once, since there is nowhere it may be
    /// kept.
    Dropped,
}

/// The result of [`Queue::notify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    pub place: Place,
    /// It updated a notification already open, so the card should change in
    /// place rather than arrive again.
    pub replaced: bool,
    /// Signals to send because of it: itself when dropped, or whatever it
    /// pushed out of a full tray.
    pub closed: Vec<Closed>,
}

/// The result of [`Queue::invoke`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub invoked: Invoked,
    /// `None` for a resident notification, which stays open.
    pub closed: Option<Closed>,
}

/// A notification that has closed, kept for the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub notification: Notification,
    pub reason: CloseReason,
    pub closed_at: Duration,
}

/// A card and its timer.
///
/// `left` is the time it has on screen, `None` for no limit. `since` is when
/// it last started counting down, `None` while it is paused, so the time
/// already spent is folded into `left` whenever it stops.
#[derive(Debug, Clone)]
struct Card {
    notification: Notification,
    left: Option<Duration>,
    since: Option<Duration>,
}

impl Card {
    fn deadline(&self) -> Option<Duration> {
        Some(self.since? + self.left?)
    }
}

/// Every open notification, and the recently closed.
///
/// Open notifications are in one of three places: a card, the tray (silent,
/// still open, still actionable), or waiting for the session to unlock.
#[derive(Debug, Clone, Default)]
pub struct Queue {
    timeouts: Timeouts,
    context: Context,
    /// Hovered, or the session idle: every card stops counting.
    paused: bool,
    /// In the order drawn: critical first, then newest first.
    cards: Vec<Card>,
    /// Newest first.
    tray: VecDeque<Notification>,
    /// In arrival order.
    waiting: Vec<Notification>,
    /// Newest first.
    history: VecDeque<Record>,
}

impl Queue {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            timeouts,
            ..Self::default()
        }
    }

    /// Change the default timeouts. Cards already on screen keep theirs.
    pub fn set_timeouts(&mut self, timeouts: Timeouts) {
        self.timeouts = timeouts;
    }

    pub fn context(&self) -> Context {
        self.context
    }

    /// Take a notification.
    ///
    /// One with the id of a notification already open replaces it where it
    /// is; a card's timer starts again, since its content is new.
    pub fn notify(&mut self, notification: Notification, now: Duration) -> Arrival {
        let id = notification.id;
        if let Some(index) = self.cards.iter().position(|c| c.notification.id == id) {
            self.cards.remove(index);
            self.insert_card(notification);
            self.settle(now);
            return Arrival {
                place: Place::Card,
                replaced: true,
                closed: Vec::new(),
            };
        }
        if let Some(slot) = self.tray.iter_mut().find(|n| n.id == id) {
            *slot = notification;
            return Arrival {
                place: Place::Tray,
                replaced: true,
                closed: Vec::new(),
            };
        }
        if let Some(slot) = self.waiting.iter_mut().find(|n| n.id == id) {
            *slot = notification;
            return Arrival {
                place: Place::Waiting,
                replaced: true,
                closed: Vec::new(),
            };
        }
        let mut closed = Vec::new();
        let place = self.place(notification, now, &mut closed);
        Arrival {
            place,
            replaced: false,
            closed,
        }
    }

    /// Tell the queue what the desktop is doing.
    ///
    /// Unlocking places everything that arrived while locked, oldest first,
    /// under the new context: the critical as cards, the rest as the policy
    /// says.
    pub fn set_context(&mut self, context: Context, now: Duration) -> Vec<Closed> {
        let was_locked = self.context.locked;
        self.context = context;
        let mut closed = Vec::new();
        if was_locked && !context.locked {
            for notification in std::mem::take(&mut self.waiting) {
                self.place(notification, now, &mut closed);
            }
        }
        self.settle(now);
        closed
    }

    /// Stop or restart every card's countdown: the pointer is over the stack,
    /// or the session has gone idle. Time away does not count.
    pub fn set_paused(&mut self, paused: bool, now: Duration) {
        if self.paused != paused {
            self.paused = paused;
            self.settle(now);
        }
    }

    /// Close the cards whose time is up.
    pub fn tick(&mut self, now: Duration) -> Vec<Closed> {
        let mut closed = Vec::new();
        while let Some(index) = self
            .cards
            .iter()
            .position(|c| c.deadline().is_some_and(|d| d <= now))
        {
            let card = self.cards.remove(index);
            // The card below comes into view and starts its own time now.
            self.settle(now);
            closed.push(self.close(card.notification, CloseReason::Expired, now));
        }
        closed
    }

    /// When [`Queue::tick`] next has something to do, for arming a timer.
    /// `None` when nothing is counting down.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.cards.iter().filter_map(Card::deadline).min()
    }

    /// The person closed it.
    pub fn dismiss(&mut self, id: Id, now: Duration) -> Option<Closed> {
        let notification = self.remove(id)?;
        self.settle(now);
        Some(self.close(notification, CloseReason::Dismissed, now))
    }

    /// Its client closed it with `CloseNotification`. `None` when it is not
    /// open, which the D-Bus side reports as an error.
    pub fn retract(&mut self, id: Id, now: Duration) -> Option<Closed> {
        let notification = self.remove(id)?;
        self.settle(now);
        Some(self.close(notification, CloseReason::Closed, now))
    }

    /// The person chose action `key`. Closes the notification unless it is
    /// resident. `None` when it is not open or has no such action.
    pub fn invoke(&mut self, id: Id, key: &str, now: Duration) -> Option<Invocation> {
        let notification = self.get(id)?;
        if !notification.actions.iter().any(|a| a.key == key) {
            return None;
        }
        let resident = notification.resident;
        let invoked = Invoked {
            id,
            key: key.to_string(),
        };
        let closed = if resident {
            None
        } else {
            // Acted on, not merely read, so it has no place in the history.
            self.remove(id);
            self.settle(now);
            Some(Closed {
                id,
                reason: CloseReason::Dismissed,
            })
        };
        Some(Invocation { invoked, closed })
    }

    /// Close everything in the tray. The person cleared it, so none of it goes
    /// to the history.
    pub fn clear_tray(&mut self) -> Vec<Closed> {
        self.tray
            .drain(..)
            .map(|n| Closed {
                id: n.id,
                reason: CloseReason::Dismissed,
            })
            .collect()
    }

    /// Bring everything in the tray back as cards, newest on top. The person
    /// asked to see what arrived quietly, so the policy that kept it quiet does
    /// not apply. Each card's time starts now. Returns how many came back.
    pub fn present_tray(&mut self, now: Duration) -> usize {
        let count = self.tray.len();
        // Oldest first, so the newest is inserted last and lands on top.
        while let Some(notification) = self.tray.pop_back() {
            self.insert_card(notification);
        }
        self.settle(now);
        count
    }

    /// How many notifications are in the tray.
    pub fn tray_len(&self) -> usize {
        self.tray.len()
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// The cards to draw, top to bottom.
    pub fn visible(&self) -> impl Iterator<Item = &Notification> {
        self.cards.iter().take(MAX_CARDS).map(|c| &c.notification)
    }

    /// Cards waiting out of view below the visible ones.
    pub fn overflow(&self) -> usize {
        self.cards.len().saturating_sub(MAX_CARDS)
    }

    /// Silent notifications, newest first.
    pub fn tray(&self) -> impl Iterator<Item = &Notification> {
        self.tray.iter()
    }

    /// How many are waiting for the session to unlock.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Closed notifications, newest first.
    pub fn history(&self) -> impl Iterator<Item = &Record> {
        self.history.iter()
    }

    /// An open notification, wherever it is.
    pub fn get(&self, id: Id) -> Option<&Notification> {
        self.cards
            .iter()
            .map(|c| &c.notification)
            .chain(self.tray.iter())
            .chain(self.waiting.iter())
            .find(|n| n.id == id)
    }

    fn place(
        &mut self,
        notification: Notification,
        now: Duration,
        closed: &mut Vec<Closed>,
    ) -> Place {
        if self.context.locked {
            self.waiting.push(notification);
            if self.waiting.len() > TRAY_LIMIT {
                let oldest = self.waiting.remove(0);
                closed.push(Closed {
                    id: oldest.id,
                    reason: CloseReason::Undefined,
                });
            }
            return Place::Waiting;
        }
        match present(notification.urgency, self.context) {
            Presentation::Show => {
                self.insert_card(notification);
                self.settle(now);
                Place::Card
            }
            Presentation::Silent if notification.transient => {
                closed.push(Closed {
                    id: notification.id,
                    reason: CloseReason::Expired,
                });
                Place::Dropped
            }
            Presentation::Silent => {
                self.tray.push_front(notification);
                while self.tray.len() > TRAY_LIMIT {
                    if let Some(oldest) = self.tray.pop_back() {
                        closed.push(Closed {
                            id: oldest.id,
                            reason: CloseReason::Undefined,
                        });
                    }
                }
                Place::Tray
            }
        }
    }

    /// Put a card in drawing order: critical cards stay in view ahead of
    /// everything else, and within each group the newest is on top.
    fn insert_card(&mut self, notification: Notification) {
        let left = self
            .timeouts
            .lifetime(notification.expire, notification.urgency);
        let index = if notification.urgency == Urgency::Critical {
            0
        } else {
            self.cards
                .iter()
                .position(|c| c.notification.urgency != Urgency::Critical)
                .unwrap_or(self.cards.len())
        };
        self.cards.insert(
            index,
            Card {
                notification,
                left,
                since: None,
            },
        );
    }

    /// Start or stop each card's countdown to match where it is now.
    ///
    /// Only a card in view counts down, and none while the queue is paused or
    /// the session locked: a card nobody could see has not been seen.
    fn settle(&mut self, now: Duration) {
        let frozen = self.paused || self.context.locked;
        for (index, card) in self.cards.iter_mut().enumerate() {
            let counting = !frozen && index < MAX_CARDS;
            match (counting, card.since) {
                (true, None) => card.since = Some(now),
                (false, Some(since)) => {
                    if let Some(left) = card.left.as_mut() {
                        *left = left.saturating_sub(now.saturating_sub(since));
                    }
                    card.since = None;
                }
                _ => {}
            }
        }
    }

    fn remove(&mut self, id: Id) -> Option<Notification> {
        if let Some(index) = self.cards.iter().position(|c| c.notification.id == id) {
            return Some(self.cards.remove(index).notification);
        }
        if let Some(index) = self.tray.iter().position(|n| n.id == id) {
            return self.tray.remove(index);
        }
        let index = self.waiting.iter().position(|n| n.id == id)?;
        Some(self.waiting.remove(index))
    }

    /// The signal for a closed notification, recording it in the history when
    /// it is worth remembering: something that expired or was dismissed, and
    /// was not transient. A client's own retraction is not.
    fn close(&mut self, notification: Notification, reason: CloseReason, now: Duration) -> Closed {
        let id = notification.id;
        if matches!(reason, CloseReason::Expired | CloseReason::Dismissed)
            && !notification.transient
        {
            self.history.push_front(Record {
                notification,
                reason,
                closed_at: now,
            });
            self.history.truncate(HISTORY_LIMIT);
        }
        Closed { id, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn note(id: Id, urgency: Urgency) -> Notification {
        let byte = match urgency {
            Urgency::Low => 0,
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        };
        let request = Request {
            summary: format!("note {id}"),
            urgency: Some(byte),
            ..Request::default()
        };
        Notification::from_request(id, request, Duration::ZERO)
    }

    fn with_actions(id: Id, resident: bool) -> Notification {
        let request = Request {
            actions: ["default", "Open", "reply", "Reply"]
                .map(String::from)
                .to_vec(),
            resident,
            ..Request::default()
        };
        Notification::from_request(id, request, Duration::ZERO)
    }

    fn visible_ids(queue: &Queue) -> Vec<Id> {
        queue.visible().map(|n| n.id).collect()
    }

    fn quiet() -> Context {
        Context {
            do_not_disturb: true,
            ..Context::default()
        }
    }

    // ---- ids ------------------------------------------------------------

    #[test]
    fn ids_start_at_one_and_never_hand_out_zero() {
        let mut ids = Ids::default();
        assert_eq!(ids.assign(0), 1);
        assert_eq!(ids.assign(0), 2);
        assert_eq!(ids.assign(u32::MAX), u32::MAX);
        assert_eq!(ids.assign(0), 1, "wrapping skips zero, which names nothing");
    }

    #[test]
    fn a_replacement_keeps_the_id_it_names_and_later_ids_skip_past_it() {
        let mut ids = Ids::default();
        assert_eq!(ids.assign(0), 1);
        assert_eq!(ids.assign(40), 40);
        assert_eq!(ids.assign(0), 41);
        assert_eq!(ids.assign(7), 7);
        assert_eq!(ids.assign(0), 42);
    }

    // ---- reading a request ------------------------------------------------

    #[test]
    fn hints_and_timeouts_are_read_as_the_specification_defines_them() {
        assert_eq!(Urgency::from_hint(0), Urgency::Low);
        assert_eq!(Urgency::from_hint(1), Urgency::Normal);
        assert_eq!(Urgency::from_hint(2), Urgency::Critical);
        assert_eq!(Urgency::from_hint(9), Urgency::Normal);
        assert_eq!(Expire::from_timeout(-1), Expire::Default);
        assert_eq!(Expire::from_timeout(i32::MIN), Expire::Default);
        assert_eq!(Expire::from_timeout(0), Expire::Never);
        assert_eq!(Expire::from_timeout(1500), Expire::After(ms(1500)));
        assert_eq!(
            Request::default().expire_timeout,
            -1,
            "a request that says nothing about timing leaves it to the server"
        );
    }

    #[test]
    fn critical_notifications_do_not_expire_by_default_but_honour_an_explicit_timeout() {
        let t = Timeouts::default();
        assert_eq!(t.lifetime(Expire::Default, Urgency::Low), Some(secs(4)));
        assert_eq!(t.lifetime(Expire::Default, Urgency::Normal), Some(secs(6)));
        assert_eq!(t.lifetime(Expire::Default, Urgency::Critical), None);
        assert_eq!(
            t.lifetime(Expire::After(secs(2)), Urgency::Critical),
            Some(secs(2))
        );
        assert_eq!(t.lifetime(Expire::Never, Urgency::Low), None);
    }

    #[test]
    fn close_reasons_use_the_specification_codes() {
        assert_eq!(CloseReason::Expired.code(), 1);
        assert_eq!(CloseReason::Dismissed.code(), 2);
        assert_eq!(CloseReason::Closed.code(), 3);
        assert_eq!(CloseReason::Undefined.code(), 4);
    }

    #[test]
    fn actions_pair_keys_with_labels_and_the_default_is_not_a_button() {
        let request = Request {
            actions: ["default", "Open", "reply", "Reply", "dangling"]
                .map(String::from)
                .to_vec(),
            ..Request::default()
        };
        let n = Notification::from_request(1, request, Duration::ZERO);
        assert_eq!(n.actions.len(), 2, "a key without a label is dropped");
        assert_eq!(n.default_action().map(|a| a.label.as_str()), Some("Open"));
        assert_eq!(
            n.buttons().map(|a| a.key.as_str()).collect::<Vec<_>>(),
            ["reply"]
        );
    }

    // ---- markup -----------------------------------------------------------

    fn style(span: &Span) -> (&str, bool, bool, bool) {
        (span.text.as_str(), span.bold, span.italic, span.underline)
    }

    #[test]
    fn plain_text_is_one_unstyled_span() {
        let spans = parse_markup("Disk is full.\nFree some space.");
        assert_eq!(spans.len(), 1);
        assert_eq!(
            style(&spans[0]),
            ("Disk is full.\nFree some space.", false, false, false)
        );
        assert!(parse_markup("").is_empty());
    }

    #[test]
    fn nested_styles_are_tracked_and_adjacent_runs_merged() {
        let spans = parse_markup("<b>bold <i>both</i></b><b></b> plain <u>under</u>");
        let got: Vec<_> = spans.iter().map(style).collect();
        assert_eq!(
            got,
            [
                ("bold ", true, false, false),
                ("both", true, true, false),
                (" plain ", false, false, false),
                ("under", false, false, true),
            ]
        );
    }

    #[test]
    fn a_link_carries_its_target_with_entities_decoded() {
        let spans =
            parse_markup(r#"see <a href="https://raven.example/?a=1&amp;b=2">the site</a> now"#);
        assert_eq!(plain(&spans), "see the site now");
        assert_eq!(spans[0].link, None);
        assert_eq!(spans[1].text, "the site");
        assert_eq!(
            spans[1].link.as_deref(),
            Some("https://raven.example/?a=1&b=2")
        );
        assert_eq!(spans[2].link, None);

        let single = parse_markup("<a target=_blank href='x.org'>x</a>");
        assert_eq!(single[0].link.as_deref(), Some("x.org"));
    }

    #[test]
    fn entities_are_decoded_and_unknown_ones_left_as_written() {
        assert_eq!(
            plain(&parse_markup(
                "a &lt; b &gt; c &amp; &quot;d&apos; &unknown; &#65;&#x42; & &;"
            )),
            "a < b > c & \"d' &unknown; AB & &;"
        );
    }

    #[test]
    fn a_less_than_sign_that_does_not_begin_a_tag_stays_text() {
        for text in ["1 < 2", "<3 you", "a <3 and b>", "a < b > c", "trailing <"] {
            assert_eq!(plain(&parse_markup(text)), text);
        }
        assert_eq!(plain(&parse_markup("x < y <b>z</b>")), "x < y z");
    }

    #[test]
    fn unknown_tags_are_stripped_and_their_text_kept() {
        assert_eq!(
            plain(&parse_markup(
                "<span foo='x'>hi</span> <BLINK>there</BLINK>"
            )),
            "hi there"
        );
    }

    #[test]
    fn an_image_becomes_its_alt_text_and_br_a_line_break() {
        assert_eq!(
            plain(&parse_markup(
                r#"line<br/>next <img src="a.png" alt="[pic]"/> <IMG SRC=b.png>"#
            )),
            "line\nnext [pic] "
        );
    }

    #[test]
    fn stray_closing_tags_do_not_count_below_zero() {
        let spans = parse_markup("</b></i>text<b>x");
        let got: Vec<_> = spans.iter().map(style).collect();
        assert_eq!(
            got,
            [("text", false, false, false), ("x", true, false, false)]
        );
    }

    #[test]
    fn multibyte_text_beside_entities_and_tags_is_read_whole() {
        assert_eq!(
            plain(&parse_markup("é&ü; 日本<b>語</b>&#x1F426;")),
            "é&ü; 日本語🐦"
        );
    }

    // ---- policy -----------------------------------------------------------

    #[test]
    fn only_critical_notifications_get_through_when_the_person_is_busy() {
        let busy = [
            quiet(),
            Context {
                fullscreen: true,
                ..Context::default()
            },
            Context {
                idle_inhibited: true,
                ..Context::default()
            },
        ];
        for context in busy {
            assert_eq!(present(Urgency::Critical, context), Presentation::Show);
            assert_eq!(present(Urgency::Normal, context), Presentation::Silent);
            assert_eq!(present(Urgency::Low, context), Presentation::Silent);
        }
        assert_eq!(
            present(Urgency::Low, Context::default()),
            Presentation::Show
        );
    }

    // ---- the queue --------------------------------------------------------

    #[test]
    fn a_new_notification_becomes_a_card_that_expires_on_time() {
        let mut queue = Queue::default();
        let arrival = queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        assert_eq!(
            arrival,
            Arrival {
                place: Place::Card,
                replaced: false,
                closed: Vec::new(),
            }
        );
        assert_eq!(queue.next_deadline(), Some(secs(6)));
        assert!(queue.tick(ms(5_999)).is_empty());
        assert_eq!(
            queue.tick(secs(6)),
            [Closed {
                id: 1,
                reason: CloseReason::Expired
            }]
        );
        assert_eq!(queue.visible().count(), 0);
        assert_eq!(queue.next_deadline(), None);
        let history: Vec<_> = queue.history().map(|r| r.notification.id).collect();
        assert_eq!(history, [1]);
    }

    #[test]
    fn a_replacement_updates_the_card_in_place_and_restarts_its_timer() {
        let mut queue = Queue::default();
        queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        let mut update = note(1, Urgency::Normal);
        update.summary = "updated".into();

        let arrival = queue.notify(update, secs(5));
        assert!(arrival.replaced);
        assert_eq!(arrival.place, Place::Card);
        assert_eq!(visible_ids(&queue), [1], "no second card");
        assert_eq!(
            queue.visible().next().map(|n| n.summary.as_str()),
            Some("updated")
        );
        assert!(
            queue.tick(secs(6)).is_empty(),
            "new content gets its full time"
        );
        assert_eq!(queue.tick(secs(11)).len(), 1);
    }

    #[test]
    fn only_the_cards_in_view_count_down() {
        let mut queue = Queue::default();
        for id in 1..=4 {
            queue.notify(note(id, Urgency::Normal), Duration::ZERO);
        }
        assert_eq!(visible_ids(&queue), [4, 3, 2]);
        assert_eq!(queue.overflow(), 1);

        queue.dismiss(4, secs(3));
        assert_eq!(visible_ids(&queue), [3, 2, 1]);
        let expired: Vec<_> = queue.tick(secs(6)).iter().map(|c| c.id).collect();
        assert_eq!(expired, [3, 2]);
        assert!(
            queue.tick(ms(8_999)).is_empty(),
            "the card that waited out of view gets its full time from when it came into view"
        );
        assert_eq!(
            queue.tick(secs(9)),
            [Closed {
                id: 1,
                reason: CloseReason::Expired
            }]
        );
    }

    #[test]
    fn critical_cards_stay_in_view_ahead_of_newer_ones() {
        let mut queue = Queue::default();
        queue.notify(note(1, Urgency::Critical), Duration::ZERO);
        for id in 2..=4 {
            queue.notify(note(id, Urgency::Normal), Duration::ZERO);
        }
        assert_eq!(visible_ids(&queue), [1, 4, 3]);
        assert_eq!(queue.overflow(), 1);

        let expired: Vec<_> = queue.tick(secs(3600)).iter().map(|c| c.id).collect();
        assert_eq!(expired, [4, 3]);
        assert_eq!(
            visible_ids(&queue),
            [1, 2],
            "the critical card is still there"
        );
    }

    #[test]
    fn hovering_holds_every_card_and_time_away_does_not_count() {
        let mut queue = Queue::default();
        queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        queue.set_paused(true, secs(2));
        assert_eq!(queue.next_deadline(), None);
        assert!(queue.tick(secs(100)).is_empty());

        queue.set_paused(false, secs(100));
        assert_eq!(
            queue.next_deadline(),
            Some(secs(104)),
            "four seconds were left"
        );
    }

    #[test]
    fn the_next_deadline_is_the_earliest_running_card() {
        let mut queue = Queue::default();
        queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        queue.notify(note(2, Urgency::Low), secs(1));
        queue.notify(note(3, Urgency::Critical), secs(1));
        assert_eq!(queue.next_deadline(), Some(secs(5)));
    }

    #[test]
    fn a_silent_notification_goes_to_the_tray_and_stays_open() {
        let mut queue = Queue::default();
        queue.set_context(quiet(), Duration::ZERO);
        let arrival = queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        assert_eq!(arrival.place, Place::Tray);
        assert_eq!(queue.visible().count(), 0);
        assert_eq!(queue.next_deadline(), None);
        assert!(
            queue.tick(secs(3600)).is_empty(),
            "nothing in the tray expires"
        );
        assert_eq!(queue.tray().map(|n| n.id).collect::<Vec<_>>(), [1]);
        assert!(queue.get(1).is_some());
    }

    #[test]
    fn a_transient_silent_notification_is_closed_rather_than_kept() {
        let mut queue = Queue::default();
        queue.set_context(quiet(), Duration::ZERO);
        let mut transient = note(1, Urgency::Normal);
        transient.transient = true;
        assert_eq!(
            queue.notify(transient, Duration::ZERO),
            Arrival {
                place: Place::Dropped,
                replaced: false,
                closed: vec![Closed {
                    id: 1,
                    reason: CloseReason::Expired
                }],
            }
        );
        assert_eq!(queue.tray().count(), 0);
        assert_eq!(queue.history().count(), 0);
    }

    #[test]
    fn while_locked_notifications_wait_and_are_placed_on_unlock() {
        let mut queue = Queue::default();
        let locked = Context {
            locked: true,
            ..quiet()
        };
        queue.set_context(locked, Duration::ZERO);
        assert_eq!(
            queue.notify(note(1, Urgency::Normal), Duration::ZERO).place,
            Place::Waiting
        );
        assert_eq!(
            queue.notify(note(2, Urgency::Critical), secs(1)).place,
            Place::Waiting
        );
        assert_eq!(queue.waiting(), 2);
        assert_eq!(queue.visible().count(), 0);
        assert!(queue.tick(secs(3600)).is_empty());

        let closed = queue.set_context(quiet(), secs(3600));
        assert!(closed.is_empty());
        assert_eq!(queue.waiting(), 0);
        assert_eq!(
            visible_ids(&queue),
            [2],
            "critical interrupts even with do not disturb on"
        );
        assert_eq!(queue.tray().map(|n| n.id).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn locking_freezes_the_cards_already_on_screen() {
        let mut queue = Queue::default();
        queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        let locked = Context {
            locked: true,
            ..Context::default()
        };
        queue.set_context(locked, secs(2));
        assert_eq!(queue.next_deadline(), None);
        assert!(queue.tick(secs(600)).is_empty());

        queue.set_context(Context::default(), secs(600));
        assert_eq!(queue.next_deadline(), Some(secs(604)));
    }

    #[test]
    fn a_full_tray_closes_its_oldest_with_reason_undefined() {
        let mut queue = Queue::default();
        queue.set_context(quiet(), Duration::ZERO);
        for id in 1..=TRAY_LIMIT {
            let id = Id::try_from(id).unwrap();
            assert!(
                queue
                    .notify(note(id, Urgency::Low), Duration::ZERO)
                    .closed
                    .is_empty()
            );
        }
        let arrival = queue.notify(note(1000, Urgency::Low), Duration::ZERO);
        assert_eq!(
            arrival.closed,
            [Closed {
                id: 1,
                reason: CloseReason::Undefined
            }]
        );
        assert_eq!(queue.tray().count(), TRAY_LIMIT);
        assert_eq!(queue.tray().next().map(|n| n.id), Some(1000));
    }

    #[test]
    fn dismissing_is_remembered_but_a_retraction_is_not() {
        let mut queue = Queue::default();
        queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        queue.notify(note(2, Urgency::Normal), Duration::ZERO);

        assert_eq!(
            queue.dismiss(1, secs(1)),
            Some(Closed {
                id: 1,
                reason: CloseReason::Dismissed
            })
        );
        assert_eq!(
            queue.retract(2, secs(1)),
            Some(Closed {
                id: 2,
                reason: CloseReason::Closed
            })
        );
        assert_eq!(queue.retract(2, secs(1)), None, "it is already gone");
        let history: Vec<_> = queue.history().map(|r| r.notification.id).collect();
        assert_eq!(history, [1]);
    }

    #[test]
    fn invoking_an_action_reports_it_and_closes_unless_resident() {
        let mut queue = Queue::default();
        queue.notify(with_actions(1, false), Duration::ZERO);
        assert_eq!(
            queue.invoke(1, "archive", Duration::ZERO),
            None,
            "no such action"
        );
        assert_eq!(
            queue.invoke(9, "reply", Duration::ZERO),
            None,
            "no such notification"
        );
        assert_eq!(visible_ids(&queue), [1]);

        let invocation = queue.invoke(1, "reply", secs(1)).unwrap();
        assert_eq!(
            invocation.invoked,
            Invoked {
                id: 1,
                key: "reply".into()
            }
        );
        assert_eq!(
            invocation.closed,
            Some(Closed {
                id: 1,
                reason: CloseReason::Dismissed
            })
        );
        assert_eq!(queue.visible().count(), 0);
        assert_eq!(queue.history().count(), 0, "acted on, so not kept");

        queue.notify(with_actions(2, true), secs(2));
        let invocation = queue.invoke(2, "default", secs(3)).unwrap();
        assert_eq!(invocation.closed, None);
        assert_eq!(visible_ids(&queue), [2], "a resident notification stays");
    }

    #[test]
    fn transient_notifications_leave_no_history() {
        let mut queue = Queue::default();
        let mut transient = note(1, Urgency::Normal);
        transient.transient = true;
        queue.notify(transient, Duration::ZERO);
        assert_eq!(queue.tick(secs(6)).len(), 1);
        assert_eq!(queue.history().count(), 0);
    }

    #[test]
    fn presenting_the_tray_brings_its_notifications_back_as_cards() {
        let mut queue = Queue::default();
        queue.set_context(quiet(), Duration::ZERO);
        queue.notify(note(1, Urgency::Normal), Duration::ZERO);
        queue.notify(note(2, Urgency::Low), Duration::ZERO);
        assert_eq!(queue.tray_len(), 2);

        assert_eq!(queue.present_tray(secs(5)), 2);
        assert_eq!(queue.tray_len(), 0);
        assert_eq!(visible_ids(&queue), [2, 1], "newest on top");
        assert_eq!(
            queue.next_deadline(),
            Some(secs(9)),
            "their time counts from when they came back"
        );
        assert_eq!(queue.present_tray(secs(6)), 0);
    }

    #[test]
    fn clearing_the_tray_closes_everything_in_it() {
        let mut queue = Queue::default();
        queue.set_context(quiet(), Duration::ZERO);
        queue.notify(note(1, Urgency::Low), Duration::ZERO);
        queue.notify(note(2, Urgency::Low), Duration::ZERO);
        let closed: Vec<_> = queue
            .clear_tray()
            .iter()
            .map(|c| (c.id, c.reason))
            .collect();
        assert_eq!(
            closed,
            [(2, CloseReason::Dismissed), (1, CloseReason::Dismissed)]
        );
        assert_eq!(queue.tray().count(), 0);
        assert_eq!(queue.history().count(), 0);
    }

    #[test]
    fn the_history_keeps_the_newest_and_can_be_cleared() {
        let mut queue = Queue::default();
        for id in 1..=60 {
            queue.notify(note(id, Urgency::Normal), Duration::ZERO);
            queue.dismiss(id, Duration::ZERO);
        }
        assert_eq!(queue.history().count(), HISTORY_LIMIT);
        assert_eq!(queue.history().next().map(|r| r.notification.id), Some(60));
        queue.clear_history();
        assert_eq!(queue.history().count(), 0);
    }
}
