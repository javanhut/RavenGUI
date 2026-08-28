//! Notice that the machine has come back from a suspend.
//!
//! A compositor that held DRM master across a suspend cannot assume the screen
//! is still where it left it. The kernel restores what it can, but the mode,
//! the framebuffer and whatever the driver was mid-flip on are not guaranteed
//! to survive; the reliable answer is to re-take the device and repaint, which
//! is exactly what already happens when a VT switch hands the session back.
//!
//! On a logind system there is a signal for this: the compositor takes an
//! inhibitor, gets `PrepareForSleep`, and is told either side. Raven has
//! seatd, which does one job — handing out devices — and knows nothing about
//! sleep. So `raven-init` publishes what it is doing to
//! `/run/raven-power/state`, one word, `sleeping` before it suspends and
//! `awake` after it returns, and this watches that file.
//!
//! # Why a file
//!
//! Because the alternative is a socket from an unprivileged session into PID 1,
//! and a world-readable word in a tmpfs carries the same information with
//! nothing to authenticate and nothing to get wrong. The compositor already
//! runs an inotify for installed applications, so watching one more directory
//! costs a descriptor.
//!
//! # Why any `awake` fires, not just one that followed a `sleeping`
//!
//! Tracking the transition looks tidier and is wrong. Everything here is frozen
//! between the two writes, so there is no guarantee this process was scheduled
//! in the moment between `sleeping` being published and the machine stopping —
//! and a resume handler that only runs if it saw the *start* of the suspend is
//! one that leaves a black screen the first time the loop was busy. So the rule
//! is the blunt one: the marker changed and it says `awake`, so repaint. The
//! cost of being wrong is one modeset, which is what a VT switch does anyway.

use std::fs;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::path::Path;

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

/// Enough for the handful of events a single rename produces.
const BUFFER: usize = 1024;

/// Watch for resumes, and run `on_resume` after each one.
///
/// Fail-soft: every early return here costs the session an automatic repaint
/// after a suspend and nothing else. A compositor that refuses to start because
/// `/run/raven-power` is missing would be a compositor that cannot run under
/// any init but ours.
pub(crate) fn watch<D, F>(handle: &LoopHandle<'static, D>, mut on_resume: F)
where
    D: 'static,
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

            if marker_changed && phase().as_deref() == Some(AWAKE) {
                tracing::info!("resumed from suspend; reclaiming the display");
                on_resume(data);
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

/// The word in the marker file, if it can be read.
fn phase() -> Option<String> {
    fs::read_to_string(Path::new(MARKER_DIR).join(MARKER_NAME))
        .ok()
        .map(|text| text.trim().to_string())
}
