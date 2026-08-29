//! Index the user's files for the launcher, off the compositor's thread.
//!
//! Walking a home directory takes anywhere from milliseconds to several
//! seconds depending on what is in it, and none of that may happen on the
//! thread that draws frames. A worker walks, hands the finished index over
//! a channel, waits, and walks again — so a file saved this morning is
//! findable by lunch without a watch on every directory under `$HOME`,
//! which is more inotify descriptors than the kernel would give us.
//!
//! The wait is not a plain sleep: the state can ask for a walk early, and
//! does so when the launcher opens on an index that has gone stale. The
//! moment the user is looking for a file is the moment its absence would be
//! noticed, and a walk that finishes while they are still typing lands as a
//! quiet update to the list.
//!
//! Fail-soft: no home, no thread, no file search. The launcher works as it
//! did before this existed.

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use calloop::LoopHandle;
use calloop::channel::{Event, channel};
use raven_desktop::FileIndex;
use raven_desktop::files::Limits;

use crate::xwayland::AsHuginn;

/// How long between walks nobody asked for. Ten minutes: a file made in the
/// last few is usually still open in whatever made it, and the walk is not
/// free.
const REFRESH: Duration = Duration::from_secs(10 * 60);

/// How old an index may be before opening the launcher asks for a new one.
/// Two minutes: long enough that reopening the launcher a few times in a
/// row does not walk the disk a few times in a row, short enough that a
/// file saved before reaching for the launcher is usually there.
const STALE: Duration = Duration::from_secs(2 * 60);

/// A request for the worker to walk now rather than at its next interval.
/// Carries nothing; the asking is the message.
pub(crate) struct WalkNow;

/// The state's end of the worker: a way to ask for a walk.
pub(crate) type Requests = mpsc::Sender<WalkNow>;

/// What was left of the request channel after draining it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Drained {
    /// The sender is still around; queued requests, if any, were discarded.
    Connected,
    /// The state dropped its handle: nothing more will ever arrive.
    Disconnected,
}

/// Throw away every queued request. Requests carry nothing, so however many
/// are waiting, one walk answers them all — and that walk either just
/// finished or is about to start.
pub(crate) fn drain(requests: &mpsc::Receiver<WalkNow>) -> Drained {
    loop {
        match requests.try_recv() {
            Ok(WalkNow) => continue,
            Err(mpsc::TryRecvError::Empty) => return Drained::Connected,
            Err(mpsc::TryRecvError::Disconnected) => return Drained::Disconnected,
        }
    }
}

/// Whether an index built at `built` deserves replacing at `now`.
///
/// `None` means no index has arrived yet — the first walk is already under
/// way, and asking again would only queue a second one behind it.
pub(crate) fn should_refresh(built: Option<Instant>, now: Instant) -> bool {
    built.is_some_and(|built| now.saturating_duration_since(built) >= STALE)
}

/// Build the index in the background and deliver it to the state.
///
/// Takes the state as well as the loop so it can leave behind the handle
/// the state pokes to ask for an early walk.
pub(crate) fn start<D>(handle: &LoopHandle<'static, D>, state: &mut crate::state::Huginn)
where
    D: AsHuginn + 'static,
{
    let Some(home) = std::env::var_os("HOME") else {
        tracing::warn!("no HOME: the launcher will not search files");
        return;
    };
    let (sender, receiver) = channel::<FileIndex>();
    let (requests, walk_now) = mpsc::channel::<WalkNow>();

    let spawned = std::thread::Builder::new()
        .name("file-index".into())
        .spawn(move || {
            loop {
                let started = Instant::now();
                let index = FileIndex::build(Path::new(&home), Limits::default());
                tracing::info!(
                    files = index.len(),
                    ms = started.elapsed().as_millis(),
                    "files indexed"
                );
                // The loop went away; so should we.
                if sender.send(index).is_err() {
                    return;
                }
                // Two drains bracket the wait. The first, here, throws away
                // requests that arrived while we were walking: the index just
                // sent is the answer to them, and leaving them queued would
                // make `recv_timeout` return at once and walk the disk again
                // for nothing. The second, after the wait, coalesces whatever
                // piled up during it into the single walk that follows.
                if drain(&walk_now) == Drained::Disconnected {
                    return;
                }
                match walk_now.recv_timeout(REFRESH) {
                    // Asked early, or the interval ran out: either way, walk.
                    Ok(WalkNow) | Err(RecvTimeoutError::Timeout) => {}
                    // The state dropped its handle; it is on the way out.
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                if drain(&walk_now) == Drained::Disconnected {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the file indexer");
        return;
    }

    let inserted = handle.insert_source(receiver, |event, _, data: &mut D| {
        if let Event::Msg(index) = event {
            data.as_huginn().set_file_index(index);
        }
    });
    if let Err(e) = inserted {
        tracing::warn!(error = %e, "could not register the file indexer");
        return;
    }
    state.set_file_index_requests(requests);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_asked_for_before_the_first_index_arrives() {
        assert!(!should_refresh(None, Instant::now()));
    }

    #[test]
    fn a_fresh_index_is_left_alone() {
        let now = Instant::now();
        assert!(!should_refresh(Some(now), now));
        assert!(!should_refresh(
            Some(now),
            now + STALE - Duration::from_secs(1)
        ));
    }

    #[test]
    fn an_index_older_than_the_stale_limit_is_refreshed() {
        let now = Instant::now();
        assert!(should_refresh(Some(now), now + STALE));
        assert!(should_refresh(Some(now), now + REFRESH));
    }

    #[test]
    fn draining_swallows_every_queued_request_and_leaves_the_channel_empty() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..3 {
            tx.send(WalkNow).unwrap();
        }
        assert_eq!(drain(&rx), Drained::Connected);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn draining_an_empty_channel_is_a_no_op() {
        let (tx, rx) = mpsc::channel::<WalkNow>();
        assert_eq!(drain(&rx), Drained::Connected);
        drop(tx);
    }

    #[test]
    fn draining_after_the_sender_is_gone_reports_disconnection() {
        let (tx, rx) = mpsc::channel();
        tx.send(WalkNow).unwrap();
        drop(tx);
        // Queued requests are still consumed before the hangup shows.
        assert_eq!(drain(&rx), Drained::Disconnected);
    }

    #[test]
    fn a_clock_that_ran_backwards_does_not_refresh() {
        // Monotonic clocks do not, but a saturating subtraction is cheaper
        // than an argument about it.
        let now = Instant::now();
        assert!(!should_refresh(Some(now + STALE), now));
    }
}
