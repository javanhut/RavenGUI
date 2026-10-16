//! xdg-activation policy: which tokens may take focus.
//!
//! A token earns focus by coming from input the person just gave: asked for
//! by the client that holds the keyboard, with a serial from this seat no
//! older than that focus. Tokens huginn hands to the applications it launches
//! are trusted too, for longer, since a cold start can take a while. Anything
//! else only marks the window as wanting attention.

use std::time::Duration;

/// Where a token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Input,
    Launch,
    Untrusted,
}

/// What an activation request is granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    Focus,
    Urgent,
}

pub(crate) const INPUT_TOKEN_LIFETIME: Duration = Duration::from_secs(10);
pub(crate) const LAUNCH_TOKEN_LIFETIME: Duration = Duration::from_secs(60);

/// Classify a client-created token.
pub(crate) fn origin(
    from_our_seat: bool,
    requester_focused: bool,
    serial_since_enter: bool,
) -> Origin {
    if from_our_seat && requester_focused && serial_since_enter {
        Origin::Input
    } else {
        Origin::Untrusted
    }
}

/// Decide an activation request made with a token of `origin`, `age` old.
pub(crate) fn grant(origin: Origin, age: Duration) -> Grant {
    let lifetime = match origin {
        Origin::Input => INPUT_TOKEN_LIFETIME,
        Origin::Launch => LAUNCH_TOKEN_LIFETIME,
        Origin::Untrusted => return Grant::Urgent,
    };
    if age <= lifetime {
        Grant::Focus
    } else {
        Grant::Urgent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_input_from_the_focused_client_on_this_seat_is_trusted() {
        assert_eq!(origin(true, true, true), Origin::Input);
        assert_eq!(origin(false, true, true), Origin::Untrusted);
        assert_eq!(origin(true, false, true), Origin::Untrusted);
        assert_eq!(origin(true, true, false), Origin::Untrusted);
    }

    #[test]
    fn fresh_trusted_tokens_focus_and_stale_ones_only_ask() {
        let s = Duration::from_secs;
        assert_eq!(grant(Origin::Input, s(1)), Grant::Focus);
        assert_eq!(grant(Origin::Input, s(11)), Grant::Urgent);
        assert_eq!(grant(Origin::Launch, s(30)), Grant::Focus);
        assert_eq!(grant(Origin::Launch, s(61)), Grant::Urgent);
        assert_eq!(grant(Origin::Untrusted, s(0)), Grant::Urgent);
    }
}
