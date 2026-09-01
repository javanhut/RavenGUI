//! Notice `desktop.toml` changing while the session runs.
//!
//! The settings application writes the file atomically — a sibling temp file
//! renamed into place — so the event that matters is `MOVED_TO` on the
//! directory, with `CLOSE_WRITE` for anyone editing it by hand. Same shape as
//! [`crate::appwatch`], and fail-soft for the same reason: losing live reload
//! costs a logout, and a compositor that will not start over an inotify
//! failure costs the machine.

use std::cell::Cell;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, LoopHandle, Mode, PostAction};
use inotify::{Inotify, WatchMask};

use crate::xwayland::AsHuginn;

/// Let a save settle before re-reading: one write is one reload.
const SETTLE: Duration = Duration::from_millis(200);

const BUFFER: usize = 4096;

pub(crate) fn start<D>(handle: &LoopHandle<'static, D>)
where
    D: AsHuginn + 'static,
{
    let Some(path) = crate::desktop_config::path() else {
        return;
    };
    let Some(dir) = path.parent().map(std::path::Path::to_owned) else {
        return;
    };
    let mut inotify = match Inotify::init() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "no inotify: settings changes need a relogin");
            return;
        }
    };
    // The directory, not the file: a rename replaces the inode, and a watch on
    // the old one would go quiet after the first save. When even the
    // directory is missing, watch its parent for it to appear; the settings
    // app creates it on first save.
    let mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE;
    let armed = inotify.watches().add(&dir, mask).is_ok()
        || dir
            .parent()
            .map(|p| {
                inotify
                    .watches()
                    .add(p, WatchMask::CREATE | WatchMask::MOVED_TO)
                    .is_ok()
            })
            .unwrap_or(false);
    if !armed {
        tracing::debug!(path = %dir.display(), "no config directory to watch");
        return;
    }
    let poll_fd = match inotify.as_fd().try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!(error = %e, "could not duplicate the inotify descriptor");
            return;
        }
    };

    let scheduled = Rc::new(Cell::new(false));
    let lh = handle.clone();
    let mut buffer = [0u8; BUFFER];
    let file_name = path.file_name().map(|n| n.to_owned());

    let inserted = handle.insert_source(
        Generic::new(poll_fd, Interest::READ, Mode::Level),
        move |_, _, _data: &mut D| {
            let mut relevant = false;
            loop {
                match inotify.read_events(&mut buffer) {
                    Ok(events) => {
                        let mut any = false;
                        for event in events {
                            any = true;
                            // Either the file itself, or the `raven` directory
                            // appearing under ~/.config (then re-arm on it).
                            match event.name {
                                Some(name) if Some(name.to_owned()) == file_name => relevant = true,
                                Some(name) if name == "raven" => {
                                    let _ = inotify.watches().add(&dir, mask);
                                    relevant = true;
                                }
                                _ => {}
                            }
                        }
                        if !any {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading inotify events");
                        break;
                    }
                }
            }
            if !relevant || scheduled.get() {
                return Ok(PostAction::Continue);
            }
            scheduled.set(true);
            let done = Rc::clone(&scheduled);
            if let Err(e) =
                lh.insert_source(Timer::from_duration(SETTLE), move |_, _, data: &mut D| {
                    done.set(false);
                    data.as_huginn().reload_desktop_config();
                    TimeoutAction::Drop
                })
            {
                tracing::warn!(error = %e, "could not schedule a settings reload");
                scheduled.set(false);
            }
            Ok(PostAction::Continue)
        },
    );
    if let Err(e) = inserted {
        tracing::warn!(error = %e, "inotify source: settings changes need a relogin");
        return;
    }
    tracing::info!(path = %path.display(), "watching the desktop settings file");
}
