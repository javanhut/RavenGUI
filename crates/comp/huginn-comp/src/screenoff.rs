//! Turning the screens off while the session is locked.
//!
//! A locked session is one nobody is using, and a lit panel showing a clock
//! is most of what a laptop spends sitting on a desk. So once the lock screen
//! has drawn, the screens go off -- straight away by default, after a delay
//! or never if `general.lock_screen_off_seconds` in `desktop.toml` says so --
//! and any input lights them again on the lock screen.
//!
//! The rule is here, without the backend, so that it can be tested; the
//! backend owns the timer and the CRTCs.

use std::time::Duration;

/// How soon screens woken on the lock screen go off again under
/// [`ScreenOff::Immediately`]. Immediately is right for a session that was
/// just locked and walked away from; for somebody who has just touched a key
/// to look at the lock screen, "immediately" would be a screen that goes dark
/// under their hands.
pub(crate) const WOKEN_GRACE: Duration = Duration::from_secs(10);

/// Input this soon after the lock began still belongs to locking it: the
/// release of `Super`+`L`, the hand coming off the touchpad. It does not count
/// as somebody at the lock screen.
pub(crate) const SETTLE: Duration = Duration::from_secs(1);

/// What `general.lock_screen_off_seconds` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ScreenOff {
    /// Leave the screens on while locked.
    Never,
    /// Off as soon as the lock screen has drawn; [`WOKEN_GRACE`] after the
    /// last input once somebody has woken it.
    #[default]
    Immediately,
    /// Off this long after the lock, or after the last input on it.
    After(Duration),
}

impl ScreenOff {
    /// `0` is immediately, a negative number is never, anything else is
    /// seconds.
    pub(crate) fn from_seconds(seconds: i64) -> Self {
        match seconds {
            s if s < 0 => Self::Never,
            0 => Self::Immediately,
            s => Self::After(Duration::from_secs(s.unsigned_abs())),
        }
    }
}

/// How long until the screens should go off, or `None` for not at all.
///
/// `since_lock` and `since_input` are measured to now; `woken` is whether
/// somebody has been at the lock screen since it went up -- input after
/// [`SETTLE`], or the lid opening on a resume. The caller only asks while
/// locked, and only once the lock screen has drawn.
///
/// `woken` is told rather than worked out from the two times because the
/// times are monotonic, which stands still in suspend: a session locked just
/// before a sleep and opened eight hours later is, to them, a lock a moment
/// old.
pub(crate) fn wait(
    policy: ScreenOff,
    since_lock: Duration,
    since_input: Duration,
    woken: bool,
) -> Option<Duration> {
    // The later of the two, counted from now.
    let since_last = since_lock.min(since_input);
    match policy {
        ScreenOff::Never => None,
        ScreenOff::Immediately => {
            if woken {
                Some(WOKEN_GRACE.saturating_sub(since_input))
            } else {
                Some(Duration::ZERO)
            }
        }
        ScreenOff::After(after) => Some(after.saturating_sub(since_last)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn the_setting_reads_as_documented() {
        assert_eq!(ScreenOff::from_seconds(0), ScreenOff::Immediately);
        assert_eq!(ScreenOff::from_seconds(-1), ScreenOff::Never);
        assert_eq!(ScreenOff::from_seconds(30), ScreenOff::After(S(30)));
        assert_eq!(ScreenOff::default(), ScreenOff::Immediately);
    }

    #[test]
    fn never_is_never() {
        assert_eq!(wait(ScreenOff::Never, S(600), S(600), true), None);
    }

    #[test]
    fn immediately_is_immediate_for_a_session_walked_away_from() {
        // Locked a moment ago; the last input was the lock chord itself.
        assert_eq!(
            wait(ScreenOff::Immediately, Duration::from_millis(300), S(5), false),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_woken_lock_screen_stays_lit_for_the_grace() {
        // Locked a minute ago, a key pressed two seconds ago.
        assert_eq!(wait(ScreenOff::Immediately, S(60), S(2), true), Some(S(8)));
        // And goes off once the grace has passed.
        assert_eq!(
            wait(ScreenOff::Immediately, S(60), S(12), true),
            Some(Duration::ZERO)
        );
        // A lid opened on a lock that, by the monotonic clock, is moments
        // old: still somebody at the machine.
        assert_eq!(
            wait(
                ScreenOff::Immediately,
                Duration::from_millis(40),
                Duration::ZERO,
                true
            ),
            Some(WOKEN_GRACE)
        );
    }

    #[test]
    fn a_delay_counts_from_the_later_of_lock_and_input() {
        let thirty = ScreenOff::After(S(30));
        assert_eq!(wait(thirty, S(10), S(600), false), Some(S(20)), "from the lock");
        assert_eq!(wait(thirty, S(600), S(10), true), Some(S(20)), "from the input");
        assert_eq!(wait(thirty, S(600), S(45), true), Some(Duration::ZERO));
    }
}
