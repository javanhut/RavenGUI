//! Notice that the machine is about to sleep, and that it has come back.
//!
//! A compositor that held DRM master across a suspend cannot assume the screen
//! is still where it left it. The kernel restores what it can, but the mode,
//! the framebuffer and whatever the driver was mid-flip on are not guaranteed
//! to survive; the reliable answer is to re-take the device and repaint, which
//! is exactly what already happens when a VT switch hands the session back.
//!
//! What the kernel *does* restore is the worse problem. The display driver
//! resumes before userspace thaws and lights the panel with whatever it was
//! scanning out when the machine stopped -- the desktop, if nothing locked it
//! first. So the session is locked and the screens turned off *before* the
//! sleep, and the machine is not allowed to go until that is done.
//!
//! On a logind system there is a signal for this: the compositor takes an
//! inhibitor, gets `PrepareForSleep`, and is told either side. Raven has
//! seatd, which does one job — handing out devices — and knows nothing about
//! sleep. So `raven-init` publishes what it is doing to
//! `/run/raven-power/state` -- `sleeping <token>` before it suspends and
//! `awake` after it returns -- and this watches that file. The answer goes
//! back as the token, written to `$XDG_RUNTIME_DIR/huginn/sleep-ready`; init
//! finds us through `$XDG_RUNTIME_DIR/huginn/pid` and waits (briefly) for it.
//!
//! # Why a file
//!
//! Because the alternative is a socket from an unprivileged session into PID 1,
//! and a world-readable word in a tmpfs carries the same information with
//! nothing to authenticate and nothing to get wrong. The compositor already
//! runs an inotify for installed applications, so watching one more directory
//! costs a descriptor. The answer is a file for the same reason in reverse:
//! init is root and can read ours, and nothing of ours has to be able to reach
//! it.
//!
//! # Why any `awake` fires, not just one that followed a `sleeping`
//!
//! Tracking the transition looks tidier and is wrong. Everything here is frozen
//! between the two writes, so there is no guarantee this process was scheduled
//! in the moment between `sleeping` being published and the machine stopping —
//! init waits for us, but only so long, and a resume handler that only runs if
//! it saw the *start* of the suspend is one that leaves a black screen the
//! first time the loop was busy. So the rule is the blunt one: the marker
//! changed and it says `awake`, so repaint. The cost of being wrong is one
//! modeset, which is what a VT switch does anyway.

use std::fs;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction};
use inotify::{Inotify, WatchMask};

/// The directory `raven-init` publishes into. Watched rather than the file
/// itself: the marker is replaced by rename, and a watch on an inode does not
/// survive the inode being replaced.
const MARKER_DIR: &str = "/run/raven-power";

/// The file within it.
const MARKER_NAME: &str = "state";

/// What it says once the machine is running again.
const AWAKE: &str = "awake";

/// What it starts with while the machine is about to sleep, before the token.
const SLEEPING: &str = "sleeping";

/// Our directory under `$XDG_RUNTIME_DIR`, which init looks through.
const RUNTIME_SUBDIR: &str = "huginn";

/// Our pid, so init knows there is a compositor to wait for.
const PID_FILE: &str = "pid";

/// The token of the sleep we are ready for.
const READY_FILE: &str = "sleep-ready";

/// What the marker says.
#[derive(Debug, PartialEq, Eq)]
enum Phase {
    /// The machine is about to sleep; answer with this token when ready.
    Sleeping(String),
    Awake,
}

/// Enough for the handful of events a single rename produces.
const BUFFER: usize = 1024;

/// Watch the marker: run `on_prepare` with the token when the machine is
/// about to sleep, and `on_resume` after each resume.
///
/// `on_prepare` owes init an answer -- [`ready`] with the token, once the
/// session is locked and the screens are off. Not answering only makes the
/// sleep wait out init's ceiling.
///
/// Fail-soft: every early return here costs the session an automatic lock and
/// repaint around a suspend and nothing else. A compositor that refuses to
/// start because `/run/raven-power` is missing would be a compositor that
/// cannot run under any init but ours.
pub(crate) fn watch<D, P, F>(handle: &LoopHandle<'static, D>, mut on_prepare: P, mut on_resume: F)
where
    D: 'static,
    P: FnMut(&mut D, String) + 'static,
    F: FnMut(&mut D) + 'static,
{
    if !Path::new(MARKER_DIR).is_dir() {
        tracing::info!(
            path = MARKER_DIR,
            "no sleep marker; the screen will not repaint itself after a suspend"
        );
        return;
    }

    let mut inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(e) => {
            tracing::warn!(error = %e, "no inotify: no repaint after a suspend");
            return;
        }
    };

    // MOVED_TO is the one that matters — init writes a temporary file and
    // renames it over the marker, so that a reader woken by the event cannot
    // catch a half-written one. CREATE covers the first publish into a
    // directory that was empty when we armed this.
    if let Err(e) = inotify
        .watches()
        .add(MARKER_DIR, WatchMask::MOVED_TO | WatchMask::CREATE)
    {
        tracing::warn!(error = %e, path = MARKER_DIR, "could not watch for resumes");
        return;
    }

    // Registered by a duplicate of the descriptor, for the reason spelled out
    // in `appwatch`: calloop hands the source back immutably and reading events
    // needs `&mut`. A dup shares the open file description, so the duplicate
    // signals readable for exactly the queue the original drains.
    let poll_fd = match inotify.as_fd().try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!(error = %e, "could not duplicate the inotify descriptor");
            return;
        }
    };

    let mut buffer = [0u8; BUFFER];

    let inserted = handle.insert_source(
        Generic::new(poll_fd, Interest::READ, Mode::Level),
        move |_, _, data: &mut D| {
            let mut marker_changed = false;

            // Drained fully before returning. The source is level-triggered,
            // so a queue left unread wakes the loop again immediately.
            loop {
                match inotify.read_events(&mut buffer) {
                    Ok(events) => {
                        let mut any = false;
                        for event in events {
                            any = true;
                            // The temporary file lands in the same directory,
                            // and its CREATE is not news. Only the marker is.
                            if event.name.is_some_and(|name| name == MARKER_NAME) {
                                marker_changed = true;
                            }
                        }
                        if !any {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading the sleep marker");
                        break;
                    }
                }
            }

            if marker_changed {
                match phase() {
                    Some(Phase::Sleeping(token)) => {
                        tracing::info!("about to sleep; locking and turning the screens off");
                        on_prepare(data, token);
                    }
                    Some(Phase::Awake) => {
                        tracing::info!("resumed from suspend; reclaiming the display");
                        on_resume(data);
                    }
                    None => {}
                }
            }

            Ok(PostAction::Continue)
        },
    );

    if let Err(e) = inserted {
        tracing::warn!(error = %e, "sleep marker source: no repaint after a suspend");
        return;
    }

    tracing::info!(path = MARKER_DIR, "watching for resumes");
}

/// Write our pid where init looks for compositors to wait on.
///
/// Fail-soft like the rest: without it init does not know to wait, and the
/// sleep goes ahead unlocked exactly as it did before the handshake existed.
pub(crate) fn announce() {
    let Some(dir) = runtime_dir() else {
        return;
    };
    let written = fs::create_dir_all(&dir)
        .and_then(|()| fs::write(dir.join(PID_FILE), format!("{}\n", std::process::id())));
    if let Err(e) = written {
        tracing::warn!(error = %e, "could not publish the pid; sleeps will not wait for the lock");
    }
}

/// Tell init this session is ready for the sleep `token` names.
pub(crate) fn ready(token: &str) {
    let Some(dir) = runtime_dir() else {
        return;
    };
    let tmp = dir.join(format!("{READY_FILE}.new"));
    let written =
        fs::write(&tmp, format!("{token}\n")).and_then(|()| fs::rename(&tmp, dir.join(READY_FILE)));
    if let Err(e) = written {
        tracing::warn!(error = %e, "could not tell init the session is ready to sleep");
    }
}

fn runtime_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join(RUNTIME_SUBDIR))
}

/// What the marker file says, if it can be read.
fn phase() -> Option<Phase> {
    parse(&fs::read_to_string(Path::new(MARKER_DIR).join(MARKER_NAME)).ok()?)
}

/// `awake`, or `sleeping` and a token. A bare `sleeping` is an init from
/// before the handshake, which is not waiting; it gets an empty token and an
/// answer nobody reads.
fn parse(text: &str) -> Option<Phase> {
    let mut words = text.split_whitespace();
    match words.next()? {
        AWAKE => Some(Phase::Awake),
        SLEEPING => Some(Phase::Sleeping(
            words.next().unwrap_or_default().to_string(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_reads_either_way_round() {
        assert_eq!(parse("awake\n"), Some(Phase::Awake));
        assert_eq!(
            parse("sleeping 123456789\n"),
            Some(Phase::Sleeping("123456789".into()))
        );
        // An init from before the handshake.
        assert_eq!(parse("sleeping\n"), Some(Phase::Sleeping(String::new())));
        assert_eq!(parse(""), None);
        assert_eq!(parse("hibernating"), None);
    }
}
