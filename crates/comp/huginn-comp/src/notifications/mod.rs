//! Notifications, served and drawn by the compositor.
//!
//! Applications notify through `org.freedesktop.Notifications` on the session
//! bus. [`bus`] answers on a thread of its own; this module is the loop's
//! side. It takes what the thread hands over, keeps it in a
//! [`huginn_core::notify::Queue`], keeps one [`Card`] per notification in
//! view, and sends back the signals clients are owed. [`card`] is how a card
//! looks and where the stack sits. The whole design is in
//! `docs/notifications.md`.
//!
//! # Time
//!
//! A card's expiry is a calloop timer, not a check made each frame. An idle
//! desktop draws no frames, and a card left on an idle desktop still has to
//! go when its time is up. One timer is armed for the earliest deadline the
//! queue reports, and armed again whenever that deadline moves earlier. A
//! timer that fires for a deadline that has since moved later finds nothing
//! due and arms the next one. Motion, on the other hand, is frames: a card
//! slides and fades while [`Notifications::tick`] says one is moving.
//!
//! # The pointer
//!
//! While the pointer is on a card, no card counts down: something being read
//! is not something to take away. A left click on a card takes it — its
//! default action if it offers one — and a click on one of its controls does
//! what the control says. A right click, or the close control, dismisses it.
//!
//! # The rest of the desktop
//!
//! What the desktop is doing decides what interrupts: do not disturb, a
//! fullscreen window and an idle inhibitor hold everything but critical
//! notifications back in the tray, where quick settings can bring them back;
//! a locked session holds everything until it unlocks. Nor does any card
//! count down while nobody is at the keyboard. See
//! [`Notifications::set_context`].

pub(crate) mod bus;
pub(crate) mod card;

use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use calloop::LoopHandle;
use calloop::channel::{self, Event};
use calloop::timer::{TimeoutAction, Timer};
use huginn_core::notify::{Closed, Context, Id, Notification, Queue, Timeouts};

use crate::anim::Reveal;
use crate::canvas::Panel;
use crate::settings::Motion;
use crate::xwayland::AsHuginn;
use bus::{Entry, Incoming, Listing, Open, Outgoing};
use card::{Hits, Target};

/// Start serving notifications.
///
/// Fail-soft like everything else that registers a source: if the source or
/// the thread cannot be started, the desktop runs exactly as before, without
/// notifications.
pub(crate) fn start<D>(handle: &LoopHandle<'static, D>, state: &mut crate::state::Huginn)
where
    D: AsHuginn + 'static,
{
    let (incoming, arrivals) = channel::channel::<Incoming>();
    let (outgoing, to_announce) = mpsc::channel::<Outgoing>();
    let open: Open = Arc::default();
    let listing: Listing = Arc::default();

    // The source goes in before the thread starts, so a bus call can never
    // arrive with nowhere to go.
    let inserted = handle.insert_source(arrivals, |event, _, data: &mut D| {
        if let Event::Msg(incoming) = event {
            data.as_huginn().notification_arrived(incoming);
        }
    });
    if let Err(e) = inserted {
        tracing::warn!(error = %e, "could not register the notifications thread");
        return;
    }

    let thread_open = Arc::clone(&open);
    let thread_listing = Arc::clone(&listing);
    // zbus calls the sink from its executor, so the sender sits behind a
    // mutex to be shareable between threads.
    let incoming = Mutex::new(incoming);
    let spawned = std::thread::Builder::new()
        .name("notifications".into())
        .spawn(move || {
            let sink = move |call| {
                incoming
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .send(call)
                    .is_ok()
            };
            bus::serve(sink, &to_announce, &thread_open, &thread_listing);
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the notifications thread");
        return;
    }

    let timers = handle.clone();
    let arm = move |after: Duration| {
        timers
            .insert_source(Timer::from_duration(after), |_, _, data: &mut D| {
                data.as_huginn().notifications_due();
                TimeoutAction::Drop
            })
            .map_err(|e| tracing::warn!(error = %e, "could not schedule a notification's expiry"))
            .is_ok()
    };
    state
        .notifications
        .attach(outgoing, open, listing, Box::new(arm));
}

/// Every notification the compositor holds, the cards that show them, and the
/// way back to the bus.
#[derive(Debug, Default)]
pub(crate) struct Notifications {
    queue: Queue,
    /// In drawing order, top to bottom, including cards on their way out.
    cards: Vec<Card>,
    /// The card under the pointer, and what on it.
    hover: Option<(Id, Target)>,
    /// Nobody has touched the keyboard or mouse for a while. No card counts
    /// down, so nothing times out unseen.
    away: bool,
    /// Absent when the server never started.
    link: Option<Link>,
    expiry: Option<Expiry>,
}

/// One card on screen, or arriving, or leaving.
#[derive(Debug)]
pub(crate) struct Card {
    id: Id,
    /// Composed by [`Notifications::compose`]; absent until then.
    panel: Option<Panel>,
    /// Where its controls are, from the same composition as the pixels.
    hits: Hits,
    reveal: Reveal,
    /// Closed, fading out, and dropped once it has.
    leaving: bool,
    /// Its notification changed, or the output did, or the pointer moved onto
    /// a different part of it: the pixels are out of date.
    stale: bool,
}

/// A card as the scene and hit testing need it.
#[derive(Debug)]
pub(crate) struct Drawn<'a> {
    pub(crate) id: Id,
    pub(crate) panel: &'a Panel,
    pub(crate) hits: &'a Hits,
    /// How far it is shown, 0..=1.
    pub(crate) shown: f32,
    /// On its way out: drawn, but no longer something to click.
    pub(crate) leaving: bool,
}

#[derive(Debug)]
struct Link {
    outgoing: mpsc::Sender<Outgoing>,
    open: Open,
    /// What the notification centre lists, kept current by
    /// [`Notifications::publish`].
    listing: Listing,
}

/// The expiry timer.
struct Expiry {
    /// Arms a timer to fire after the given wait. `false` if it could not.
    arm: Box<dyn Fn(Duration) -> bool>,
    /// The deadline the latest timer was armed for.
    armed: Option<Duration>,
}

impl std::fmt::Debug for Expiry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expiry")
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

impl Notifications {
    fn attach(
        &mut self,
        outgoing: mpsc::Sender<Outgoing>,
        open: Open,
        listing: Listing,
        arm: Box<dyn Fn(Duration) -> bool>,
    ) {
        self.link = Some(Link {
            outgoing,
            open,
            listing,
        });
        self.expiry = Some(Expiry { arm, armed: None });
    }

    /// Act on a call from the bus.
    pub(crate) fn receive(&mut self, incoming: Incoming, now: Duration, motion: Motion) {
        match incoming {
            Incoming::Notify {
                id,
                request,
                sender,
            } => {
                // The application, not the text: what a notification says
                // does not belong in the log at the default level.
                tracing::info!(
                    id,
                    app = %request.app_name,
                    sender = sender.as_deref().unwrap_or("-"),
                    "notification"
                );
                let notification = Notification::from_request(id, *request, now);
                let arrival = self.queue.notify(notification, now);
                tracing::debug!(
                    id,
                    place = ?arrival.place,
                    replaced = arrival.replaced,
                    "notification placed"
                );
                if arrival.replaced {
                    self.mark_stale(id);
                }
                self.announce(arrival.closed);
            }
            Incoming::Close(id) => {
                let closed = self.queue.retract(id, now);
                self.announce(closed);
            }
            Incoming::Remove(id) => {
                // Dismissing puts it in the history, which is exactly where
                // the person just asked it not to be.
                let closed = self.queue.dismiss(id, now);
                self.announce(closed);
                self.queue.forget(id);
            }
            Incoming::Clear => {
                let ids: Vec<Id> = self.queue.open().map(|n| n.id).collect();
                let closed: Vec<Closed> = ids
                    .into_iter()
                    .filter_map(|id| self.queue.dismiss(id, now))
                    .collect();
                self.announce(closed);
                self.queue.clear_history();
            }
        }
        self.settle(now, motion);
    }

    /// The expiry timer fired: close whatever is due, and arm the next one.
    pub(crate) fn due(&mut self, now: Duration, motion: Motion) {
        if let Some(expiry) = &mut self.expiry {
            expiry.armed = None;
        }
        let closed = self.queue.tick(now);
        self.announce(closed);
        self.settle(now, motion);
    }

    /// The person closed a notification. Returns whether it was open.
    pub(crate) fn dismiss(&mut self, id: Id, now: Duration, motion: Motion) -> bool {
        let Some(closed) = self.queue.dismiss(id, now) else {
            return false;
        };
        self.announce([closed]);
        self.settle(now, motion);
        true
    }

    /// The person chose action `key` of a notification. Returns whether the
    /// notification was open and had that action.
    ///
    /// `ActionInvoked` goes before `NotificationClosed`, the order a client
    /// expects: what was chosen, then that the notification is gone.
    pub(crate) fn invoke(&mut self, id: Id, key: &str, now: Duration, motion: Motion) -> bool {
        let Some(invocation) = self.queue.invoke(id, key, now) else {
            return false;
        };
        tracing::debug!(id, key, "notification action");
        if let Some(link) = &self.link {
            let _ = link.outgoing.send(Outgoing::Invoked(invocation.invoked));
        }
        self.announce(invocation.closed);
        self.settle(now, motion);
        true
    }

    /// Dismiss the card on top. Returns whether there was one.
    pub(crate) fn dismiss_newest(&mut self, now: Duration, motion: Motion) -> bool {
        let Some(id) = self.queue.visible().next().map(|n| n.id) else {
            return false;
        };
        self.dismiss(id, now, motion)
    }

    /// Dismiss every card, including those waiting out of view. The tray is
    /// left alone: what arrived quietly is still there to read. Returns
    /// whether there was anything to dismiss.
    pub(crate) fn dismiss_all(&mut self, now: Duration, motion: Motion) -> bool {
        let mut any = false;
        while self.dismiss_newest(now, motion) {
            any = true;
        }
        any
    }

    /// An open notification.
    pub(crate) fn get(&self, id: Id) -> Option<&Notification> {
        self.queue.get(id)
    }

    /// The key of the action a card draws as its `index`th control.
    pub(crate) fn button_key(&self, id: Id, index: usize) -> Option<String> {
        self.queue
            .get(id)?
            .buttons()
            .nth(index)
            .map(|action| action.key.clone())
    }

    /// Tell the cards where the pointer is. Returns whether that changed
    /// anything, in which case the cards concerned need composing again.
    pub(crate) fn set_hover(&mut self, hover: Option<(Id, Target)>, now: Duration) -> bool {
        if self.hover == hover {
            return false;
        }
        for (id, _) in [self.hover, hover].into_iter().flatten() {
            self.mark_stale(id);
        }
        self.hover = hover;
        self.queue
            .set_paused(self.hover.is_some() || self.away, now);
        self.rearm(now);
        true
    }

    /// Tell the notifications what the desktop is doing, and whether anybody
    /// is at it. Returns whether that changed anything.
    ///
    /// Asked every frame, so it does nothing unless something differs. See
    /// [`huginn_core::notify::present`] for what each part of the context
    /// holds back.
    pub(crate) fn set_context(
        &mut self,
        context: Context,
        away: bool,
        now: Duration,
        motion: Motion,
    ) -> bool {
        let mut changed = false;
        if self.queue.context() != context {
            let closed = self.queue.set_context(context, now);
            self.announce(closed);
            changed = true;
        }
        if self.away != away {
            self.away = away;
            changed = true;
        }
        if changed {
            self.queue
                .set_paused(self.hover.is_some() || self.away, now);
            self.settle(now, motion);
        }
        changed
    }

    /// How long cards stay by default, from `desktop.toml`. Cards on screen
    /// keep the time they were given.
    pub(crate) fn set_timeouts(&mut self, timeouts: Timeouts) {
        self.queue.set_timeouts(timeouts);
    }

    /// How many notifications arrived quietly and are waiting to be seen.
    pub(crate) fn tray_len(&self) -> usize {
        self.queue.tray_len()
    }

    /// Bring what arrived quietly back as cards. Returns whether there was
    /// anything.
    pub(crate) fn present_tray(&mut self, now: Duration, motion: Motion) -> bool {
        if self.queue.present_tray(now) == 0 {
            return false;
        }
        self.settle(now, motion);
        true
    }

    /// Every card's pixels are out of date: the output, or its density,
    /// changed.
    pub(crate) fn invalidate(&mut self) {
        for card in &mut self.cards {
            card.stale = true;
        }
    }

    /// Compose every card that has no pixels or out-of-date ones. Returns
    /// whether any was composed. `render` is given the notification and what
    /// on its card the pointer is over.
    ///
    /// A leaving card keeps the pixels it has: its notification is gone from
    /// the queue, and it only needs to look as it did while it fades.
    pub(crate) fn compose(
        &mut self,
        mut render: impl FnMut(&Notification, Option<Target>) -> (Panel, Hits),
    ) -> bool {
        let hover = self.hover;
        let mut composed = false;
        for card in &mut self.cards {
            if card.leaving || !(card.stale || card.panel.is_none()) {
                continue;
            }
            if let Some(notification) = self.queue.get(card.id) {
                let target = hover
                    .filter(|(id, _)| *id == card.id)
                    .map(|(_, target)| target);
                let (panel, hits) = render(notification, target);
                card.panel = Some(panel);
                card.hits = hits;
                card.stale = false;
                composed = true;
            }
        }
        composed
    }

    /// Move the cards on: drop the ones that have finished leaving. Returns
    /// whether a frame is owed, for a card still moving or one just dropped.
    pub(crate) fn tick(&mut self, now: Duration) -> bool {
        let before = self.cards.len();
        self.cards
            .retain(|card| !(card.leaving && card.reveal.is_settled(now)));
        before != self.cards.len() || self.cards.iter().any(|card| !card.reveal.is_settled(now))
    }

    /// The cards that have pixels, in drawing order, as they are at `now`.
    pub(crate) fn drawn(&self, now: Duration) -> impl Iterator<Item = Drawn<'_>> {
        self.cards.iter().filter_map(move |card| {
            Some(Drawn {
                id: card.id,
                panel: card.panel.as_ref()?,
                hits: &card.hits,
                shown: card.reveal.value(now),
                leaving: card.leaving,
            })
        })
    }

    /// How many items [`Self::drawn`] yields: what the scene pushes for the
    /// cards, counted without building them.
    pub(crate) fn drawn_count(&self) -> usize {
        self.cards
            .iter()
            .filter(|card| card.panel.is_some())
            .count()
    }

    fn mark_stale(&mut self, id: Id) {
        for card in &mut self.cards {
            if card.id == id && !card.leaving {
                card.stale = true;
            }
        }
    }

    /// After the queue changed: bring the cards into line with it, and the
    /// expiry timer and the notification centre's list too.
    fn settle(&mut self, now: Duration, motion: Motion) {
        self.sync(now, motion);
        self.rearm(now);
        self.publish(now);
    }

    /// Give the notification centre the list as it is now: open notifications
    /// and the history, newest first. `Changed` goes out only when the list is
    /// different, so a card merely coming into view says nothing.
    fn publish(&self, now: Duration) {
        let Some(link) = &self.link else {
            return;
        };
        let open = self.queue.open().map(|n| (n, true));
        let closed = self.queue.history().map(|r| (&r.notification, false));
        let mut entries: Vec<Entry> = open
            .chain(closed)
            .map(|(n, open)| Entry {
                id: n.id,
                app_name: n.app_name.clone(),
                app_icon: n.app_icon.clone(),
                summary: n.summary.clone(),
                body: huginn_core::notify::plain(&n.body),
                arrived: n.arrived,
                open,
            })
            .collect();
        // Stable, so notifications that arrived together keep the queue's
        // order.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.arrived));

        let mut snapshot = link.listing.lock().unwrap_or_else(PoisonError::into_inner);
        snapshot.now = now;
        snapshot.taken = std::time::Instant::now();
        if snapshot.entries != entries {
            snapshot.entries = entries;
            drop(snapshot);
            let _ = link.outgoing.send(Outgoing::Changed);
        }
    }

    /// Make the cards match what the queue has in view: a new card for each
    /// notification that came into view, sliding in, and the cards of the ones
    /// that left sliding out from where they were.
    fn sync(&mut self, now: Duration, motion: Motion) {
        let instant = motion.is_reduced();
        let visible: Vec<Id> = self.queue.visible().map(|n| n.id).collect();

        let mut staying = Vec::new();
        let mut leaving = Vec::new();
        for (index, mut card) in std::mem::take(&mut self.cards).into_iter().enumerate() {
            if !card.leaving && visible.contains(&card.id) {
                staying.push(card);
            } else {
                if !card.leaving {
                    card.leaving = true;
                    card.reveal.close(now, instant);
                }
                leaving.push((index, card));
            }
        }

        let mut cards: Vec<Card> = visible
            .iter()
            .map(|&id| match staying.iter().position(|card| card.id == id) {
                Some(index) => staying.swap_remove(index),
                None => {
                    let mut reveal = Reveal::hidden();
                    reveal.open(now, instant);
                    Card {
                        id,
                        panel: None,
                        hits: Hits::default(),
                        reveal,
                        leaving: false,
                        stale: false,
                    }
                }
            })
            .collect();
        // A leaving card fades where it was, rather than jumping to the end.
        for (index, card) in leaving {
            let at = index.min(cards.len());
            cards.insert(at, card);
        }
        self.cards = cards;
    }

    /// Arm the expiry timer for the queue's earliest deadline, unless one is
    /// already armed for that deadline or an earlier one.
    fn rearm(&mut self, now: Duration) {
        let (Some(expiry), Some(deadline)) = (&mut self.expiry, self.queue.next_deadline()) else {
            return;
        };
        if expiry.armed.is_some_and(|armed| armed <= deadline) {
            return;
        }
        if (expiry.arm)(deadline.saturating_sub(now)) {
            expiry.armed = Some(deadline);
        }
    }

    /// Tell clients which notifications closed, and stop counting them as
    /// open.
    fn announce(&self, closed: impl IntoIterator<Item = Closed>) {
        let Some(link) = &self.link else {
            return;
        };
        for c in closed {
            link.open
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&c.id);
            tracing::debug!(id = c.id, reason = c.reason.code(), "notification closed");
            // A thread that is gone has nobody left to tell.
            let _ = link.outgoing.send(Outgoing::Closed(c));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Canvas;
    use huginn_core::notify::{CloseReason, Invoked, Request};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn notify(id: u32, summary: &str) -> Incoming {
        notify_with(id, summary, &[])
    }

    fn notify_with(id: u32, summary: &str, actions: &[&str]) -> Incoming {
        Incoming::Notify {
            id,
            request: Box::new(Request {
                summary: summary.into(),
                actions: actions.iter().map(|a| a.to_string()).collect(),
                ..Request::default()
            }),
            sender: Some(":1.42".into()),
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Stands in for `card::render`: the card's pixels, without fonts.
    fn blank(_: &Notification, _: Option<Target>) -> (Panel, Hits) {
        (Panel::from_canvas(&Canvas::new(4, 4), 1), Hits::default())
    }

    /// Notifications with a server attached, the signals it would send, and
    /// every wait the expiry timer was armed for.
    fn attached() -> (
        Notifications,
        mpsc::Receiver<Outgoing>,
        Open,
        Rc<RefCell<Vec<Duration>>>,
    ) {
        let (notifications, announced, open, armed, _) = attached_with_listing();
        (notifications, announced, open, armed)
    }

    /// What [`attached`] gives, and the notification centre's listing too.
    type AttachedWithListing = (
        Notifications,
        mpsc::Receiver<Outgoing>,
        Open,
        Rc<RefCell<Vec<Duration>>>,
        Listing,
    );

    fn attached_with_listing() -> AttachedWithListing {
        let (outgoing, announced) = mpsc::channel();
        let open: Open = Arc::default();
        let listing: Listing = Arc::default();
        let armed = Rc::new(RefCell::new(Vec::new()));
        let record = Rc::clone(&armed);
        let mut notifications = Notifications::default();
        notifications.attach(
            outgoing,
            Arc::clone(&open),
            Arc::clone(&listing),
            Box::new(move |after| {
                record.borrow_mut().push(after);
                true
            }),
        );
        (notifications, announced, open, armed, listing)
    }

    fn closed(id: Id, reason: CloseReason) -> Outgoing {
        Outgoing::Closed(Closed { id, reason })
    }

    /// Everything sent so far, as text, for comparing whole sequences. The
    /// centre's `Changed` is left out, except where a test asks for it with
    /// [`sent_all`].
    fn sent(announced: &mpsc::Receiver<Outgoing>) -> Vec<String> {
        announced
            .try_iter()
            .filter(|o| !matches!(o, Outgoing::Changed))
            .map(|o| format!("{o:?}"))
            .collect()
    }

    fn sent_all(announced: &mpsc::Receiver<Outgoing>) -> Vec<String> {
        announced.try_iter().map(|o| format!("{o:?}")).collect()
    }

    /// The centre's list: each id and whether it is still open.
    fn listed(listing: &Listing) -> Vec<(Id, bool)> {
        listing
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.id, e.open))
            .collect()
    }

    #[test]
    fn the_centre_lists_open_and_closed_newest_first_and_says_when_it_changes() {
        let (mut notifications, announced, _, _, listing) = attached_with_listing();
        notifications.receive(notify(1, "one"), secs(1), Motion::Reduced);
        notifications.receive(notify(2, "two"), secs(2), Motion::Reduced);
        notifications.dismiss(1, secs(3), Motion::Reduced);
        assert_eq!(listed(&listing), [(2, true), (1, false)]);
        assert_eq!(
            sent_all(&announced)
                .iter()
                .filter(|s| *s == "Changed")
                .count(),
            3,
            "one Changed for each change"
        );

        notifications.set_hover(Some((2, Target::Body)), secs(4));
        notifications.due(secs(5), Motion::Reduced);
        assert!(
            sent_all(&announced).is_empty(),
            "nothing listed changed, so nothing is said"
        );
    }

    #[test]
    fn removing_from_the_centre_dismisses_and_keeps_no_history() {
        let (mut notifications, announced, _, _, listing) = attached_with_listing();
        notifications.receive(notify(1, "one"), secs(1), Motion::Reduced);
        notifications.receive(notify(2, "two"), secs(2), Motion::Reduced);
        notifications.dismiss(1, secs(3), Motion::Reduced);
        let _ = sent_all(&announced);

        notifications.receive(Incoming::Remove(2), secs(4), Motion::Reduced);
        assert_eq!(
            sent(&announced),
            [format!("{:?}", closed(2, CloseReason::Dismissed))],
            "its client is told it was dismissed"
        );
        assert_eq!(listed(&listing), [(1, false)]);

        notifications.receive(Incoming::Remove(1), secs(5), Motion::Reduced);
        assert!(listed(&listing).is_empty(), "a closed one is forgotten");
        assert!(sent(&announced).is_empty(), "and nobody is owed a signal");
    }

    #[test]
    fn clearing_the_centre_dismisses_everything_and_forgets_the_history() {
        let (mut notifications, announced, _, _, listing) = attached_with_listing();
        for id in 1..=5 {
            notifications.receive(notify(id, "hello"), secs(1), Motion::Reduced);
        }
        notifications.dismiss(1, secs(2), Motion::Reduced);
        let _ = sent_all(&announced);

        notifications.receive(Incoming::Clear, secs(3), Motion::Reduced);
        assert_eq!(sent(&announced).len(), 4, "the four still open");
        assert!(listed(&listing).is_empty());
        notifications.compose(blank);
        notifications.tick(secs(3));
        assert_eq!(notifications.drawn_count(), 0);
    }

    #[test]
    fn a_retraction_is_announced_and_no_longer_counted_as_open() {
        let (mut notifications, announced, open, _) = attached();
        open.lock().unwrap().insert(1);

        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        assert!(sent(&announced).is_empty(), "nothing closes on arrival");

        notifications.receive(Incoming::Close(1), secs(1), Motion::Full);
        assert_eq!(
            sent(&announced),
            [format!("{:?}", closed(1, CloseReason::Closed))]
        );
        assert!(!open.lock().unwrap().contains(&1));
    }

    #[test]
    fn closing_something_already_gone_says_nothing() {
        let (mut notifications, announced, _, _) = attached();
        notifications.receive(Incoming::Close(7), Duration::ZERO, Motion::Full);
        assert!(sent(&announced).is_empty());
    }

    #[test]
    fn without_a_server_notifications_are_still_held() {
        let mut notifications = Notifications::default();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        notifications.receive(Incoming::Close(1), Duration::ZERO, Motion::Full);
        assert!(notifications.get(1).is_none());
    }

    #[test]
    fn a_notification_gets_a_card_that_is_drawn_once_composed() {
        let mut notifications = Notifications::default();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        assert_eq!(
            notifications.drawn_count(),
            0,
            "nothing to draw before composing"
        );

        assert!(notifications.compose(blank));
        assert_eq!(notifications.drawn_count(), 1);
        assert!(
            !notifications.compose(blank),
            "a composed card is not composed again"
        );
        assert!(notifications.tick(Duration::ZERO), "it is still sliding in");
    }

    #[test]
    fn cards_follow_the_queue_order_newest_on_top() {
        let mut notifications = Notifications::default();
        for id in 1..=3 {
            notifications.receive(notify(id, "hello"), Duration::ZERO, Motion::Reduced);
        }
        let order: Vec<Id> = notifications.cards.iter().map(|card| card.id).collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[test]
    fn a_closed_card_fades_where_it_was_and_is_dropped_once_gone() {
        let mut notifications = Notifications::default();
        for id in 1..=3 {
            notifications.receive(notify(id, "hello"), Duration::ZERO, Motion::Full);
        }
        notifications.compose(blank);
        notifications.receive(Incoming::Close(2), secs(1), Motion::Full);

        let order: Vec<(Id, bool)> = notifications
            .drawn(secs(1))
            .map(|card| (card.id, card.leaving))
            .collect();
        assert_eq!(order, [(3, false), (2, true), (1, false)]);
        assert_eq!(notifications.drawn_count(), 3, "still drawn while it fades");

        assert!(notifications.tick(secs(10)), "dropping it owes a frame");
        assert_eq!(notifications.drawn_count(), 2);
        assert!(!notifications.tick(secs(11)), "and then nothing is moving");
    }

    #[test]
    fn with_reduced_motion_cards_arrive_and_leave_at_once() {
        let mut notifications = Notifications::default();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Reduced);
        notifications.compose(blank);
        assert!(
            notifications
                .drawn(Duration::ZERO)
                .all(|card| card.shown == 1.0)
        );

        notifications.receive(Incoming::Close(1), Duration::ZERO, Motion::Reduced);
        notifications.tick(Duration::ZERO);
        assert_eq!(notifications.drawn_count(), 0);
    }

    #[test]
    fn a_replacement_recomposes_its_card_in_place() {
        let mut notifications = Notifications::default();
        notifications.receive(notify(1, "first"), Duration::ZERO, Motion::Full);
        notifications.compose(blank);

        notifications.receive(notify(1, "second"), secs(1), Motion::Full);
        assert_eq!(notifications.cards.len(), 1, "no second card");
        let mut summaries = Vec::new();
        assert!(notifications.compose(|n, hover| {
            summaries.push(n.summary.clone());
            blank(n, hover)
        }));
        assert_eq!(summaries, ["second"]);
    }

    #[test]
    fn expiry_is_armed_for_the_earliest_deadline_and_closes_on_time() {
        let (mut notifications, announced, _, armed) = attached();

        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        assert_eq!(
            *armed.borrow(),
            [secs(6)],
            "armed for the default six seconds"
        );

        notifications.receive(notify(2, "hello"), secs(1), Motion::Full);
        assert_eq!(
            armed.borrow().len(),
            1,
            "a later deadline needs no new timer"
        );

        notifications.due(secs(6), Motion::Full);
        assert_eq!(
            sent(&announced),
            [format!("{:?}", closed(1, CloseReason::Expired))]
        );
        assert_eq!(
            *armed.borrow(),
            [secs(6), secs(1)],
            "then armed for the next one"
        );

        notifications.due(secs(7), Motion::Full);
        assert_eq!(
            sent(&announced),
            [format!("{:?}", closed(2, CloseReason::Expired))]
        );
        assert_eq!(armed.borrow().len(), 2, "nothing left to wait for");
    }

    #[test]
    fn a_timer_that_fires_early_finds_nothing_due_and_waits_again() {
        let (mut notifications, announced, _, armed) = attached();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        notifications.due(secs(2), Motion::Full);
        assert!(sent(&announced).is_empty());
        assert_eq!(*armed.borrow(), [secs(6), secs(4)]);
    }

    #[test]
    fn a_card_under_the_pointer_does_not_expire() {
        let (mut notifications, announced, _, armed) = attached();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        notifications.compose(blank);

        assert!(notifications.set_hover(Some((1, Target::Body)), secs(2)));
        assert!(
            !notifications.set_hover(Some((1, Target::Body)), secs(3)),
            "nothing new"
        );
        notifications.due(secs(6), Motion::Full);
        assert!(sent(&announced).is_empty(), "held while pointed at");

        assert!(notifications.set_hover(None, secs(60)));
        assert_eq!(
            armed.borrow().last(),
            Some(&secs(4)),
            "the four seconds it had left"
        );
    }

    #[test]
    fn moving_onto_a_different_part_of_a_card_recomposes_it_with_the_hover() {
        let mut notifications = Notifications::default();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        notifications.compose(blank);

        notifications.set_hover(Some((1, Target::Close)), Duration::ZERO);
        let mut seen = Vec::new();
        assert!(notifications.compose(|n, hover| {
            seen.push(hover);
            blank(n, hover)
        }));
        assert_eq!(seen, [Some(Target::Close)]);
    }

    #[test]
    fn dismissing_closes_with_reason_dismissed() {
        let (mut notifications, announced, _, _) = attached();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);
        assert!(notifications.dismiss(1, secs(1), Motion::Full));
        assert!(
            !notifications.dismiss(1, secs(1), Motion::Full),
            "already gone"
        );
        assert_eq!(
            sent(&announced),
            [format!("{:?}", closed(1, CloseReason::Dismissed))]
        );
    }

    #[test]
    fn an_action_is_announced_before_the_close_it_causes() {
        let (mut notifications, announced, _, _) = attached();
        notifications.receive(
            notify_with(1, "hello", &["default", "Open", "reply", "Reply"]),
            Duration::ZERO,
            Motion::Full,
        );
        assert_eq!(notifications.button_key(1, 0).as_deref(), Some("reply"));
        assert_eq!(notifications.button_key(1, 1), None);

        assert!(!notifications.invoke(1, "archive", secs(1), Motion::Full));
        assert!(notifications.invoke(1, "reply", secs(1), Motion::Full));
        let invoked = Outgoing::Invoked(Invoked {
            id: 1,
            key: "reply".into(),
        });
        assert_eq!(
            sent(&announced),
            [
                format!("{invoked:?}"),
                format!("{:?}", closed(1, CloseReason::Dismissed))
            ]
        );
    }

    #[test]
    fn while_locked_nothing_is_shown_and_unlocking_shows_what_arrived() {
        let mut notifications = Notifications::default();
        let locked = Context {
            locked: true,
            ..Context::default()
        };
        assert!(notifications.set_context(locked, false, Duration::ZERO, Motion::Reduced));
        assert!(
            !notifications.set_context(locked, false, Duration::ZERO, Motion::Reduced),
            "unchanged"
        );

        notifications.receive(notify(1, "hello"), secs(1), Motion::Reduced);
        notifications.compose(blank);
        assert_eq!(notifications.drawn_count(), 0);

        notifications.set_context(Context::default(), false, secs(2), Motion::Reduced);
        notifications.compose(blank);
        assert_eq!(notifications.drawn_count(), 1);
    }

    #[test]
    fn do_not_disturb_holds_notifications_back_until_they_are_asked_for() {
        let mut notifications = Notifications::default();
        let quiet = Context {
            do_not_disturb: true,
            ..Context::default()
        };
        notifications.set_context(quiet, false, Duration::ZERO, Motion::Reduced);
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Reduced);
        notifications.compose(blank);
        assert_eq!(notifications.drawn_count(), 0);
        assert_eq!(notifications.tray_len(), 1);

        assert!(notifications.present_tray(secs(1), Motion::Reduced));
        notifications.compose(blank);
        assert_eq!(notifications.drawn_count(), 1);
        assert_eq!(notifications.tray_len(), 0);
        assert!(
            !notifications.present_tray(secs(1), Motion::Reduced),
            "nothing left"
        );
    }

    #[test]
    fn time_away_from_the_desktop_does_not_count() {
        let (mut notifications, announced, _, armed) = attached();
        notifications.receive(notify(1, "hello"), Duration::ZERO, Motion::Full);

        assert!(notifications.set_context(Context::default(), true, secs(2), Motion::Full));
        notifications.due(secs(6), Motion::Full);
        assert!(sent(&announced).is_empty(), "nobody was there to see it go");

        assert!(notifications.set_context(Context::default(), false, secs(600), Motion::Full));
        assert_eq!(
            armed.borrow().last(),
            Some(&secs(4)),
            "the four seconds it had left"
        );
    }

    #[test]
    fn dismissing_everything_clears_the_cards_out_of_view_too() {
        let (mut notifications, announced, _, _) = attached();
        for id in 1..=5 {
            notifications.receive(notify(id, "hello"), Duration::ZERO, Motion::Full);
        }
        assert!(notifications.dismiss_newest(secs(1), Motion::Full));
        assert_eq!(
            sent(&announced),
            [format!("{:?}", closed(5, CloseReason::Dismissed))]
        );

        assert!(notifications.dismiss_all(secs(1), Motion::Full));
        assert_eq!(sent(&announced).len(), 4);
        assert!(
            !notifications.dismiss_all(secs(1), Motion::Full),
            "nothing left"
        );
    }
}
